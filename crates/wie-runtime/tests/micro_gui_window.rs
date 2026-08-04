//! GUI micro-test: checks that a PE with window creation + paint does not crash.
//!
//! Requires mingw-built gui_*.exe (run `make -C micro-exes gui_exes`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// Locate a real (non-micro) guest binary under `real_exes/` — e.g. the
/// RNotepad build fetched by `scripts/fetch-rnotepad.sh`. `None` when the
/// binary is absent (gitignored; the test skips).
fn real_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("real_exes");
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
                std::thread::sleep(std::time::Duration::from_millis(50));
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

#[test]
fn gui_edit_multiline_edit_messages() {
    let Some(path) = micro_exe("gui_edit.exe") else {
        eprintln!("skip: micro-exes/out/gui_edit.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe's selftest drives eight assertion groups against a multiline
    // EDIT (SetWindowText + EM_GETLINECOUNT, EM_LINEFROMCHAR, EM_SETSEL/
    // EM_GETSEL, EM_REPLACESEL, EM_GETMODIFY, WM_COPY →
    // IsClipboardFormatAvailable, EM_UNDO, WM_SETTEXT line-count reset). The
    // first failed group exits with 300..307 (group code + 200) so CI
    // pinpoints the broken message; exit 0 only happens when every group
    // passed, so it proves the EDIT message core end-to-end.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_edit.exe must exit 0 (proves the multiline EDIT selftest groups passed); \
         got {exit_code:?}"
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

/// In-process repro for the interactive notepad menubar regression: every
/// File-menu action (New/Open/Save/Save As/Exit) does nothing in the real
/// app. The host bar delivers the click by posting `WM_COMMAND(id, 0)` to the
/// main window's hwnd — this test posts the SAME message with the ids the bar
/// itself stamps (the parsed `window_menu_items` tree) and checks whether the
/// guest reacts. If the guest exits here, the posted-message → WndProc
/// dispatch works and the break is host-side (the muda MenuEvent → proxy →
/// user_event delivery). If not, the guest dispatch is broken.
#[test]
fn notepad_menu_command_reaches_the_guest_wndproc() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!(
            "skip: real_exes/notepad.exe not present (fetch with ./scripts/fetch-rnotepad.sh)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();

    // Pump until notepad's main window exists and its menu is loaded (the
    // bar would mirror it via window_menu_items).
    let mut main_hwnd = 0;
    for _ in 0..300 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop");
        if let Some(hwnd) = session.first_guest_window_handle() {
            main_hwnd = hwnd;
            if !handle.window_menu_items().is_empty() {
                break;
            }
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }
    assert_ne!(main_hwnd, 0, "notepad must create its main window");
    assert!(
        !handle.window_menu_items().is_empty(),
        "notepad must load its menu (the bar mirrors it)"
    );

    // The File menu's Exit command — the ids the macOS bar stamps into the
    // native items and decodes back on click.
    let tree = handle.window_menu_items();
    let exit_id = tree
        .iter()
        .find_map(|top| {
            top.children
                .iter()
                .find(|child| child.title.to_lowercase().contains("xit"))
                .map(|child| child.id)
        })
        .unwrap_or(0);
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Post the exact WM_COMMAND the MenuEvent handler posts.
    handle.post_message(main_hwnd, WM_COMMAND, u64::from(exit_id), 0);

    let mut exited = false;
    for _ in 0..150 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop after WM_COMMAND");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            exited = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        exited,
        "WM_COMMAND(CMD_EXIT={exit_id}) posted to the main window must make \
         notepad exit — the guest dispatch is broken if it idles on"
    );
}

/// Control for the Exit reaction: an UNKNOWN command id (999, not in the
/// menu) must NOT make notepad exit — otherwise the Exit test's reaction was
/// "any WM_COMMAND exits", not a real CMD_EXIT dispatch.
#[test]
fn notepad_ignores_unknown_command_ids() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();

    for _ in 0..300 {
        let _ = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, 999, 0);

    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!(
                "notepad exited on an unknown WM_COMMAND id — the Exit test is a false positive"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// L4 rebuild-gate diagnostic: `window_menu_items` must return a STABLE tree
/// for a single-window guest (same Arc from the cache), so the host bar's
/// per-Frame `sync_menu_bar` never rebuilds (and never tears down the native
/// menu mid-click). A focus move between windows is the only intended rebuild
/// trigger.
#[test]
fn notepad_menu_tree_is_stable_across_calls() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();

    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() && !handle.window_menu_items().is_empty() {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }

    let first = handle.window_menu_items();
    let mut saw_cache_hit = false;
    let mut saw_content_change = false;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = session.run_until_stop(1_000_000).expect("run");
        let next = handle.window_menu_items();
        if Arc::ptr_eq(&first, &next) {
            saw_cache_hit = true;
        }
        if next.as_ref() != first.as_ref() {
            saw_content_change = true;
        }
    }
    assert!(
        saw_cache_hit,
        "an idle single-window guest must hit the menu-tree cache (same Arc) \
         so the bar never rebuilds per frame — the menu_dirty flag must be \
         re-armed after the tree is consumed (regression: it was never reset, \
         so every call rebuilt and the native bar was torn down repeatedly)"
    );
    assert!(
        !saw_content_change,
        "an idle guest's menu tree must not change content between calls — \
         a changing tree would rebuild the native bar every Frame (and tear \
         it down mid-click)"
    );
}

/// Control for [`notepad_menu_command_reaches_the_guest_wndproc`]: an idle
/// notepad session must NOT exit on its own within the same pumping budget.
/// If this exits, the repro test's "guest reacted" was a false positive (the
/// runtime's idle-exit, not the WM_COMMAND).
#[test]
fn notepad_does_not_exit_while_idle() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();

    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() && !handle.window_menu_items().is_empty() {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }

    // Same budget as the repro's post-WM_COMMAND pump — but NO message posted.
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!("notepad exited while idle — the WM_COMMAND repro is a false positive");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// File-menu action repros (real_exes/notepad.exe): the interactive host
// delivery is proven (MenuEvent → WM_COMMAND works, Exit reacts); these pin
// whether the guest ACTIONS complete. CMD ids verified against the RT_MENU
// 0x201 template: New=256, New Window=257, Open=258, Save=259, Save As=260.
// ---------------------------------------------------------------------------

/// Drive one of notepad's File commands on a live session and return the
/// window records after the action settles (so the caller can assert on the
/// resulting state: a FileDialog window for Open/SaveAs, the save prompt for
/// New on a dirty doc, etc.).
fn pump_until_windows_ready(session: &mut wie_runtime::RuntimeSession) {
    use wie_runtime::EntryTraceTermination;
    let handle = session.guest_handle();
    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() && !handle.window_menu_items().is_empty() {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its window was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }
}

/// Wait up to ~7.5 s for a window whose class equals `class` to appear.
fn wait_for_window_class(
    session: &mut wie_runtime::RuntimeSession,
    handle: &wie_runtime::GuestHandle,
    class: &str,
) -> bool {
    use wie_runtime::EntryTraceTermination;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            return false;
        }
        if session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == class)
        {
            return true;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = handle;
    }
    false
}

/// WM_COMMAND(CMD_OPEN) with the interactive file dialog enabled must make
/// the guest call GetOpenFileNameW and BUILD the host dialog (a
/// "FileDialog"-class window appears). If the dialog fails to build live
/// (the bottle-confinement suspect), no such window ever appears.
#[test]
fn notepad_file_open_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    // Dismiss the dialog if it appeared (IDCANCEL) so the session can end.
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_OPEN must build the interactive file dialog (a FileDialog window) — \
         the action does not complete if no dialog appears"
    );
    let _ = EntryTraceTermination::WaitingForMessage;
}

/// WM_COMMAND(CMD_SAVE_AS) with the interactive policy must build the dialog
/// too (Save As always prompts).
#[test]
fn notepad_file_save_as_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_SAVE_AS: u32 = 260;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE_AS), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_SAVE_AS must build the interactive file dialog — the action does \
         not complete if no dialog appears"
    );
}

/// The font dialog (ChooseFontW, comdlg32) must SURVIVE a click on one of
/// its controls: the click's repaint publishes a frame that still carries the
/// dialog's pixels in the OWNER surface (the dialog composites into its
/// owner — it has no winit window of its own).
///
/// In-process repro of the reported "the dialog becomes invisible if I click
/// on it": drive notepad's Format > Font (the REAL menu → WM_COMMAND path),
/// wait for the dialog face in the owner's published frame, click the family
/// LISTBOX through the host hit-test + posting path (exactly what app.rs does
/// for winit mouse events), then require the face to STAY in the owner frame
/// across the click's repaint cycle. The click must select the listbox row,
/// not erase the dialog.
#[test]
fn notepad_font_dialog_survives_control_click() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: u64 = 0x0001;
    // The font dialog's size (comdlg32): the dialog is centered in the owner
    // and its controls sit at fixed offsets inside it.
    const FONT_DLG_CX: i32 = 340;
    const FONT_DLG_CY: i32 = 260;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_font_dialog_policy(wie_winapi::FontDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    // The Format > Font command id (the guest's own menu tree, like the bar).
    let font_id = handle
        .window_menu_items()
        .iter()
        .find_map(|top| {
            top.children
                .iter()
                .find(|child| child.title.to_lowercase().contains("font"))
                .map(|child| child.id)
        })
        .unwrap_or(0);
    assert_ne!(font_id, 0, "the Format menu must contain a Font command");

    // The font dialog is centered in the owner; its face sample is 5 px in
    // from the dialog's top-left corner (clear of the 1 px border and the
    // "Font:" label at x=8).
    let (_hwnd, _title, owner_w, owner_h) =
        handle
            .first_guest_window_info()
            .unwrap_or((0, String::new(), 0, 0));
    let dx = owner_w.saturating_sub(FONT_DLG_CX).saturating_div(2);
    let dy = owner_h.saturating_sub(FONT_DLG_CY).saturating_div(2);
    let face_px_at = |frame: &wie_winapi::present::SurfaceFrame| {
        let x = usize::try_from(dx + 5).unwrap_or(0);
        let y = usize::try_from(dy + 5).unwrap_or(0);
        frame.pixels.get(y * frame.width as usize + x).copied()
    };

    handle.post_message(main, WM_COMMAND, u64::from(font_id), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FontDialog");
    assert!(
        opened,
        "Format > Font must build the font dialog (a FontDialog window)"
    );

    // The family LISTBOX is at dialog (72,8,168,140); click row 2's band.
    let click_owner_x = dx + 72 + 40;
    let click_owner_y = dy + 8 + 40;

    let mut saw_face_before_click = false;
    let mut clicked = false;
    let mut lost_face_after_click = false;
    let mut saw_sel_change_effect = false;
    let mut settle_after_effect = 0;
    let mut iterations = 0;
    loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop");
        iterations += 1;
        if iterations >= 800 {
            let face_now = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| face_px_at(&f));
            let (fw, fh) = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| (f.width, f.height))
                .unwrap_or((0, 0));
            // Where is BTNFACE in the frame? The first few rows that contain
            // it tell us where the dialog actually sits.
            let rows: Vec<u32> = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| {
                    (0..fh)
                        .filter(|y| {
                            f.pixels.get(*y as usize * f.width as usize).copied()
                                == Some(BTNFACE_0RGB)
                        })
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            panic!(
                "notepad font-dialog click session stalled: \
                 saw_face={saw_face_before_click} clicked={clicked} \
                 lost_face={lost_face_after_click} effect={saw_sel_change_effect} \
                 face_now={face_now:?} frame={fw}x{fh} bfnface_col0_rows={rows:?} \
                 font_dialog_open={}",
                session
                    .guest_windows_snapshot()
                    .iter()
                    .any(|(_, cls, ..)| cls == "FontDialog")
            );
        }
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            break;
        }

        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            let face = face_px_at(&frame);
            if face == Some(BTNFACE_0RGB) {
                saw_face_before_click = true;
            }
            if clicked && saw_face_before_click && face != Some(BTNFACE_0RGB) {
                // The dialog's face was in the owner frame and the click's
                // repaint removed it — the reported click-invisibility.
                lost_face_after_click = true;
            }
            if clicked {
                // The clicked row must be highlighted in the listbox area
                // (the selection followed the click through the repaint).
                let highlight = (dy + 8..dy + 8 + 140).fold(0_u32, |acc, y| {
                    acc + (dx + 72..dx + 72 + 168).fold(0_u32, |acc, x| {
                        let idx = usize::try_from(y).unwrap_or(0) * frame.width as usize
                            + usize::try_from(x).unwrap_or(0);
                        acc + u32::from(frame.pixels.get(idx).copied() == Some(0x0000_78D7))
                    })
                });
                if highlight > 100 {
                    saw_sel_change_effect = true;
                }
            }
        }

        if !clicked && saw_face_before_click {
            // The dialog is visible: click the family LISTBOX (host path).
            if let Some((hwnd, rx, ry)) = handle.window_at(click_owner_x, click_owner_y)
                && hwnd != 0
                && rx < 168
                && ry < 140
            {
                let lparam = u64::from(ry << 16 | rx);
                handle.post_message_at(
                    hwnd,
                    WM_LBUTTONDOWN,
                    MK_LBUTTON,
                    lparam,
                    click_owner_x,
                    click_owner_y,
                );
                handle.post_message_at(hwnd, WM_LBUTTONUP, 0, lparam, click_owner_x, click_owner_y);
                clicked = true;
            }
        }

        // Once the click's repaint published (the highlight is visible), let
        // a few more frames settle, then verify the dialog face survived.
        if clicked && saw_sel_change_effect {
            settle_after_effect += 1;
            if settle_after_effect >= 5 {
                break;
            }
        }

        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    assert!(
        saw_face_before_click,
        "the font dialog face (0xF0F0F0) never appeared in the owner frame"
    );
    assert!(
        !lost_face_after_click,
        "the font dialog face disappeared from the owner frame after a click \
         on the family listbox — the click-invisibility regression"
    );
    assert!(
        saw_sel_change_effect,
        "the click must select + highlight a listbox row (the selection \
         followed the click)"
    );
}

/// WM_COMMAND(CMD_NEW) must not kill the session (no emulation error), even
/// on a doc with typed text — the save-prompt path must fire without the
/// guest stopping. On a clean doc FileNew is a no-op; the assertion here is
/// that the session survives the command.
#[test]
fn notepad_file_new_survives_with_typed_text() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_NEW: u32 = 256;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    // Record the save-prompt MessageBox instead of answering it: FileNew on a
    // dirty doc must call MessageBoxW (the prompt). The bridge returns IDNO
    // (discard) so the action completes.
    let prompt_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt = std::sync::Arc::clone(&prompt_fired);
    handle.set_message_box_bridge(Box::new(move |_, _, _| {
        prompt.store(true, std::sync::atomic::Ordering::SeqCst);
        7 // IDNO — discard the changes
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    // Type into the EDIT so the doc is dirty (FileNew must offer to save).
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    for c in "hello".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }

    // Drain the typed text, then trigger FileNew.
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);

    // The session must keep running (the save prompt / FileNew completes);
    // an emulation error (the missing SHUFPD bug) stops it with RuntimeStop.
    let mut kept_running = true;
    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(msg) => {
                kept_running = false;
                eprintln!("DIAG NEW RuntimeStop: {msg}");
                break;
            }
            other => {
                eprintln!("DIAG NEW other: {other:?}");
            }
        }
    }
    assert!(
        kept_running,
        "CMD_NEW on a dirty doc must not stop the session (the save-prompt \
         path runs) — an emulation error here is the missing-instruction bug"
    );
    assert!(
        prompt_fired.load(std::sync::atomic::Ordering::SeqCst),
        "FileNew on a dirty doc must invoke the save-prompt MessageBox bridge — \
         if the prompt never fires, the action does not complete"
    );
}

/// WM_COMMAND(CMD_NEW_WINDOW) must fire ShellExecuteW without stopping the
/// session. ShellExecuteW is a shell32 stub; the command must at least not
/// crash the guest.
#[test]
fn notepad_file_new_window_does_not_stop_the_session() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_NEW_WINDOW: u32 = 257;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW_WINDOW), 0);

    let mut kept_running = true;
    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(msg) => {
                kept_running = false;
                eprintln!("DIAG NEW_WINDOW RuntimeStop: {msg}");
                break;
            }
            other => {
                eprintln!("DIAG NEW_WINDOW other: {other:?}");
            }
        }
    }
    assert!(
        kept_running,
        "CMD_NEW_WINDOW must not stop the session (ShellExecuteW runs) — an \
         emulation error here is the missing-instruction bug"
    );
}

/// The interactive File→Open dialog's first paint must reach the OWNER's
/// published surface while the dialog is open.
///
/// The FileDialog (like the FontDialog) is PARENTED to its owner — it has no
/// winit window of its own and composites into the owner's surface — so its
/// face pixels appear in `published[owner]`, never in a frame keyed by the
/// dialog hwnd. This pins the paint → publish seam of the "dialog invisible
/// until hover/click" report: the dialog's frame reliably reaches the owner's
/// published surface, so the reported loss is in the host present path (a
/// surface-acquire skip marking a frame presented without drawing it), not in
/// the guest paint path.
#[test]
fn notepad_file_dialog_paints_into_the_owner_surface() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;
    // The file dialog is 360×200 (comdlg32 FILE_DLG_CX/CY), centered in the
    // owner's client. The sample is dialog-local (3,33) — the BTNFACE face
    // margin clear of the 1 px border, the EDIT (8,8,344,22) and the LISTBOX
    // (8,36,344,116) — resolved to owner coordinates from the frame size.
    const FILE_DLG_CX: i32 = 360;
    const FILE_DLG_CY: i32 = 200;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let mut saw_dialog_record = false;
    let mut saw_dialog_face_in_owner = false;
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            saw_dialog_record = true;
        }
        if let Some(frame) = session.take_frame(main) {
            // The dialog is centered in the owner: (owner - 360x200) / 2.
            let dx = (i32::try_from(frame.width).unwrap_or(0) - FILE_DLG_CX).max(0) / 2;
            let dy = (i32::try_from(frame.height).unwrap_or(0) - FILE_DLG_CY).max(0) / 2;
            let (sx, sy) = (
                u32::try_from(dx.saturating_add(3)).unwrap_or(0),
                u32::try_from(dy.saturating_add(33)).unwrap_or(0),
            );
            if sx < frame.width && sy < frame.height {
                let idx = usize::try_from(sy).unwrap_or(0) * frame.width as usize
                    + usize::try_from(sx).unwrap_or(0);
                if frame.pixels.get(idx).copied() == Some(BTNFACE_0RGB) {
                    saw_dialog_face_in_owner = true;
                }
            }
        }
        match summary.termination {
            wie_runtime::EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            wie_runtime::EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {}
        }
        if saw_dialog_record && saw_dialog_face_in_owner {
            break;
        }
    }

    // Dismiss the dialog so the session can wind down (the assertion ran
    // while it was open).
    if dialog_hwnd != 0 {
        handle.post_message(dialog_hwnd, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    assert!(
        saw_dialog_record,
        "CMD_OPEN must build the interactive file dialog (a FileDialog window record)"
    );
    assert!(
        saw_dialog_face_in_owner,
        "the file dialog's BTNFACE must appear in the OWNER's published frame \
         while the dialog is open — the dialog composites into the owner surface, \
         so an invisible dialog means a lost host present, not a missing paint"
    );
}

/// WM_COMMAND(CMD_SAVE) on an untitled doc must build the Save As dialog
/// (the guest has no filename yet, so Save routes to GetSaveFileName).
#[test]
fn notepad_file_save_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_SAVE: u32 = 259;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_SAVE on an untitled doc must build the interactive Save As dialog \
         — the action does not complete if no dialog appears"
    );
}

/// The ghost-modal regression: after the interactive file dialog closes (OK),
/// the FIRST File→Exit click must make the guest exit.
///
/// Reported live: after closing any in-app modal (File→Open/Save, Format→Font)
/// the dialog visually closes but a GHOST modal state persists — File→Exit
/// needs TWO clicks. This drives the exact sequence through the real guest
/// (notepad): CMD_OPEN → interactive FileDialog → CMD_OPEN's modal loop →
/// OK (EndDialog) → modal loop exits → ONE CMD_EXIT → guest must exit.
#[test]
fn notepad_file_dialog_close_then_first_exit_click_exits() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    // The in-app (host-built) file dialog — what the GUI presenter enables.
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    // The File menu's Open and Exit ids (like the menu-bar decode does).
    let open_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("pen"))
        .map(|child| child.id)
        .unwrap_or(0);
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(open_id, 0, "the File menu must contain an Open command");
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Stage 1: File→Open builds the interactive dialog and parks the guest's
    // in-guest modal loop on an empty queue (dialog_depth == 1).
    handle.post_message(main, WM_COMMAND, u64::from(open_id), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_OPEN must build the interactive file dialog"
    );

    // Stage 2: OK closes the dialog (the guest's modal loop consumes the
    // WM_QUIT EndDialog posted — the depth returns to 0).
    handle.post_message(dialog_hwnd, WM_COMMAND, 1, 0); // IDOK
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog")
        {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the file dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }
    assert!(
        !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog"),
        "OK must close the file dialog (EndDialog removes the subtree)"
    );

    // Stage 3: ONE File→Exit after the dialog is gone. A ghost modal state
    // (stale dialog_depth or a leftover dialog window) swallows this first
    // command — the guest idles on instead of exiting.
    handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);

    let mut exited = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            exited = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        exited,
        "the FIRST File→Exit click after closing the modal dialog must make \
         notepad exit — a swallowed first command means a ghost modal state persists"
    );
}

/// Same sequence as [`notepad_file_dialog_close_then_first_exit_click_exits`],
/// but the final Exit command is delivered through the REAL GUI pump —
/// [`run_windowed`] parked on the message-signal condvar, woken by a host
/// thread's post (exactly what `wie-cli run --gui` does). The direct-drive
/// test above proves the emulation; this one proves the GUI pump does not
/// lose the first post after a modal dialog closes.
#[test]
fn notepad_modal_dialog_exit_survives_run_windowed_pump() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    let open_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("pen"))
        .map(|child| child.id)
        .unwrap_or(0);
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(open_id, 0, "the File menu must contain an Open command");
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Open the dialog, OK it, and wait until the subtree is gone.
    handle.post_message(main, WM_COMMAND, u64::from(open_id), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_OPEN must build the interactive file dialog"
    );
    handle.post_message(dialog_hwnd, WM_COMMAND, 1, 0); // IDOK
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog")
        {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the file dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }

    // The GUI's MenuEvent analog: a host thread posts ONE CMD_EXIT while the
    // pump is parked on the message-signal condvar.
    let poster = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);
    });

    let control = wie_runtime::GuiControl::new();
    let outcome = wie_runtime::run_windowed(&mut session, &control)
        .expect("run_windowed after the modal dialog must succeed");
    poster.join().expect("exit poster thread");

    assert!(
        matches!(outcome, wie_runtime::GuiOutcome::Exited(0)),
        "run_windowed must return Exited after ONE post-close CMD_EXIT; got {outcome:?}"
    );
}

/// The ghost-modal regression through the FONT dialog and the REAL host click
/// path: Format→Font opens the in-app font dialog, a host-posted
/// WM_LBUTTONDOWN/UP on its OK button closes it via the control → BN_CLICKED →
/// dialog-proc → EndDialog chain (exactly what the live GUI mouse produces),
/// and the FIRST File→Exit after the close must make the guest exit.
#[test]
fn notepad_font_dialog_ok_click_then_first_exit_exits() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: u64 = 0x0001;
    const CMD_FONT: u32 = 320; // Format→Font...

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    // The in-app font dialog — always the in-guest modal loop (no native
    // bridge exists for ChooseFontW).
    session.set_font_dialog_policy(wie_winapi::FontDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Stage 1: Format→Font builds the font dialog and parks the modal loop.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_FONT), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FontDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the font dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_FONT must build the interactive font dialog"
    );

    // Stage 2: host-posted click on the OK button (a child of the dialog).
    let ok_hwnd = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, _cls, title, _)| title == "OK")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(ok_hwnd, 0, "the font dialog must have an OK button");
    // The button's client rect is 80×24; click its center.
    let lparam = u64::from(u32::try_from((12 << 16) | 40).unwrap_or(0));
    handle.post_message(ok_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam);
    handle.post_message(ok_hwnd, WM_LBUTTONUP, 0, lparam);

    // The dialog must close (EndDialog removes the subtree).
    let mut closed = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FontDialog")
        {
            closed = true;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the font dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }
    assert!(
        closed,
        "the OK click must close the font dialog (EndDialog removes the subtree)"
    );

    // Stage 3: ONE File→Exit after the dialog is gone.
    handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);
    let mut exited = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            exited = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        exited,
        "the FIRST File→Exit click after the font dialog closes must make \
         notepad exit — a swallowed first command means a ghost modal state"
    );
}
