//! Shared helpers for the GUI micro-tests: PE lookup, the suite-wide
//! serialization lock, session driving, frame hashing, and the pump/wait/ink
//! primitives the notepad and dialog tests build on.
//!
//! Everything here is `pub(crate)`: the test binary is one crate and the
//! sibling test modules (`controls`, `demo`, `dialogs`, `notepad`) call these
//! across the mod tree.

use std::path::{Path, PathBuf};

/// Locate a mingw-built micro PE under `micro-exes/out`. `None` when the
/// binary is absent (the test skips).
pub(crate) fn micro_exe(name: &str) -> Option<PathBuf> {
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
pub(crate) fn real_exe(name: &str) -> Option<PathBuf> {
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
pub(crate) static GUI_SUITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the suite-wide serialization lock (see [`GUI_SUITE_LOCK`]).
///
/// Held for the whole test; the guard's `Drop` runs on unwinding too, so a
/// panicking test cannot deadlock its successors (the next `lock()` sees a
/// poisoned mutex and recovers via `PoisonError::into_inner`).
pub(crate) fn gui_suite_serialize() -> std::sync::MutexGuard<'static, ()> {
    GUI_SUITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drives a YieldOnIdle GUI session like the persistent GUI loop, sleeping a
/// short quantum between empty-queue yields so the host clock advances and
/// WM_TIMER / synthesized WM_PAINT actually fire.
pub(crate) fn drive_gui_session(path: &Path, iterations_budget: usize) -> Option<u32> {
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
pub(crate) fn frame_hash(frame: &wie_winapi::present::SurfaceFrame, top: u32, bottom: u32) -> u64 {
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
pub(crate) const GUI_BLIT_RESTING_FRAME_HASH: u64 = 0x9FB3_7202_941F_08DA;

/// `COLOR_BTNFACE` — the classic push-button / dialog face color.
pub(crate) const BTNFACE_0RGB: u32 = 0x00F0_F0F0;
/// White text glyph color used by gui_blit's DIB text (SetTextColor white).
pub(crate) const TEXT_WHITE_0RGB: u32 = 0x00FF_FFFF;

/// D3D9 clear color from gui_d3d9's `D3DCOLOR_XRGB(200,0,0)` → 0RGB.
pub(crate) const D3D9_CLEAR_RED_0RGB: u32 = 0x00C8_0000;
/// D3D9 solid cyan from gui_d3d9's indexed triangle
/// (`D3DCOLOR_XRGB(0,255,255)` → 0RGB).
pub(crate) const D3D9_CYAN_0RGB: u32 = 0x0000_FFFF;
/// D3D9 textured-quad texel colors as rendered: MODULATE with an opaque white
/// diffuse multiplies `(255*255)>>8 = 254`, so the 2x2 checkerboard texels
/// land as 0xFE-red / 0xFE-green / 0xFE-blue / 0xFE-white.
pub(crate) const D3D9_QUAD_RED_0RGB: u32 = 0x00FE_0000;
pub(crate) const D3D9_QUAD_GREEN_0RGB: u32 = 0x0000_FE00;
pub(crate) const D3D9_QUAD_BLUE_0RGB: u32 = 0x0000_00FE;
pub(crate) const D3D9_QUAD_WHITE_0RGB: u32 = 0x00FE_FEFE;
/// P4c alpha-blend result at (30,35): blue (alpha 0x80) over red with
/// SRCALPHA/INVSRCALPHA ADD → ((src*128 + dst*127) >> 8) per channel.
pub(crate) const D3D9_BLEND_0RGB: u32 = 0x007E_007F;
/// P4c opaque red quad (no blending) at (80,35).
pub(crate) const D3D9_RED_QUAD_0RGB: u32 = 0x00FF_0000;
/// P4c far depth quad (magenta, z=0.9) where the near quad does not cover.
pub(crate) const D3D9_FAR_DEPTH_0RGB: u32 = 0x00FF_00FF;
/// P4c near depth quad (white, z=0.1) — wins the depth test in the overlap.
pub(crate) const D3D9_NEAR_DEPTH_0RGB: u32 = 0x00FF_FFFF;
/// L3 alpha-test quad's passing half (alpha 0x80 > ALPHAREF 0x40): white.
pub(crate) const D3D9_ALPHA_PASS_0RGB: u32 = 0x00FF_FFFF;
/// L3 fogged quad: red under blue LINEAR fog at f=0.5 → (128, 0, 128).
pub(crate) const D3D9_FOGGED_0RGB: u32 = 0x0080_0080;
/// L3 scissor quad inside the scissor rect: solid green.
pub(crate) const D3D9_SCISSOR_INSIDE_0RGB: u32 = 0x0000_FF00;

/// CI-gated hash of gui_d3d9's deterministic resting frame (320×240).
///
/// The frame is the clear-red backbuffer with the gradient triangle, the cyan
/// indexed triangle, the 2x2 textured quad, the P4c alpha-blended quads, the
/// P4c depth-tested quads, the L1 vs_2_0-driven textured quad, the L1
/// w-skewed quad whose perspective-correct center pixel (165,175) samples the
/// RED texel (affine interpolation would sample GREEN), and the L3
/// fragment-stage strip at y∈[220,235] (the alpha-tested quad, the fogged
/// quad, the scissor-clipped quad). Recompute with
/// `./target/debug/wie-cli run --screenshot /tmp/d.bmp \
/// micro-exes/out/gui_d3d9.exe` then FNV-1a 64 over the full 0RGB pixel bytes.
/// Recomputed when the L3 lane added the fog/alpha-test/scissor strip (the
/// deliberate D3D9_RESTING_FRAME_HASH tripwire for the fragment-stage change).
pub(crate) const D3D9_RESTING_FRAME_HASH: u64 = 0x07EA_21E2_A9C9_4062;

/// Verify the deterministic frame's pixel content: the gradient survives the
/// WS_CLIPCHILDREN-clipped blit, the DIB text drew white glyph ink, and the
/// button (BTNFACE + border) and static (WINDOW fill) children rendered at
/// their fixed positions. The gradient formula matches gui_blit/main.c:
/// r=(x*255)/1280, g=(y*255)/800, b=((x+y)*127)/2080.
pub(crate) fn assert_frame_renders_gradient_text_and_controls(
    frame: &wie_winapi::present::SurfaceFrame,
) {
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

/// Owner-client coordinates of the gui_dialog OK-button face (template id 100:
/// "100 DIALOG 10,20,160,60", OK at DLU (10,10,60,20) → px (20,20,120,40),
/// dialog centered in the 1280×800 owner at (480,340) → button at (500,360)).
pub(crate) const DIALOG_OK_BUTTON_SAMPLE: (u32, u32) = (508, 368);

/// Drive one of notepad's File commands on a live session and return the
/// window records after the action settles (so the caller can assert on the
/// resulting state: a FileDialog window for Open/SaveAs, the save prompt for
/// New on a dirty doc, etc.).
pub(crate) fn pump_until_windows_ready(session: &mut wie_runtime::RuntimeSession) {
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
pub(crate) fn wait_for_window_class(
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

/// Count non-COLOR_WINDOW (non-white) pixels in the top band of the owner's
/// published frame — the region where the multiline EDIT's first text line
/// renders (RNotepad moves the EDIT to (0,0) and it fills the client, so the
/// text starts near the top-left; the status bar is at the bottom).
///
/// The band starts at x=8 to skip the EDIT's left border/margin and the caret
/// bar (which sits at the text origin, x≈2..5), so a cleared edit reads ~0
/// even when the caret is drawn.
pub(crate) fn count_edit_ink(session: &wie_runtime::RuntimeSession, owner: u64) -> u32 {
    let Some(frame) = session.take_frame(owner) else {
        return 0;
    };
    let mut ink = 0_u32;
    for y in 2..40_u32 {
        for x in 8..400_u32 {
            if x < frame.width && y < frame.height {
                let idx = usize::try_from(y).unwrap_or(0) * frame.width as usize
                    + usize::try_from(x).unwrap_or(0);
                if frame.pixels.get(idx).copied() != Some(0x00FF_FFFF) {
                    ink += 1;
                }
            }
        }
    }
    ink
}
