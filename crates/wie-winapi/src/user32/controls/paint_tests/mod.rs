//! Pixel-level tests for the relocated control-paint paths.
//!
//! The control tree split (`paint.rs` / `button/` / `statusbar/` out of the
//! old 1,113-line `button.rs`) was validated behaviour-preserving by
//! byte-comparing ONE frame of ONE application (`scripts/notepad-scenario.sh`).
//! That single frame is not coverage: the pressed-button face, the rect-level
//! repaint scope, the LISTBOX row colouring, the COMBOBOX paint and the EDIT
//! row-band invalidation all moved files without a single direct assertion.
//!
//! ## Why this directory
//!
//! The relocated code is spread over `paint.rs`, `button/mod.rs`,
//! `listbox.rs`, `edit/` and `statusbar/mod.rs`, so the tests live in a
//! `controls/tests/` sibling directory rather than inside any one of them —
//! the same shape `state/tests/` uses for the crate's other themed test
//! groups. That keeps every module under the 2,000-line cap (`paint.rs` is
//! already at 995 lines) and lets each submodule sit beside the path it pins.
//!
//! ## The fixtures
//!
//! Everything goes through the REAL creation path
//! ([`crate::user32::create_window_record`]) and the REAL
//! [`crate::user32::controls::dispatch_control_proc`] entry point, so
//! `control_kind`, the visibility flag and the control-state seed all come
//! from production code — a test cannot accidentally pass against a
//! hand-poked window record.
//!
//! Assertions are pixel assertions against the published ancestor surface
//! (there is no golden-image framework in this crate and none was added): read
//! a frame with [`published`] and compare [`frame_pixel`] to the expected
//! `GetSysColor` value.
//!
//! The one technique worth calling out is [`sentinel`]: it writes a colour
//! that appears nowhere in the control palette straight into the ancestor
//! surface, so a subsequent repaint can be asked "which pixels did you
//! actually overwrite?". Without it a partial-repaint test can only compare
//! frames before/after, which cannot distinguish "erased exactly the pending
//! rect" from "erased nothing" when the erase colour happens to equal what was
//! already there. The sentinel makes the erase boundary directly observable.

#![allow(clippy::expect_used)]

use crate::WinApiState;
use crate::gdi32::IRect;
use crate::present::SurfaceFrame;
use crate::state::tests::winapi_state_default_with_bump_heap;
use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

mod button_face;
mod combo_box;
mod dirty_rect;
mod edit_band;
mod listbox_rows;
mod status_bar_paint;

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x_0001_0000;
/// `STACK_VA + STACK_SIZE - 0x100` — room for a dummy return address.
const STACK_TOP: u64 = 0x100_FF00;

/// A guest address the tests write ANSI item strings at (inside the mapped
/// 0x1000 page `test_engine` creates, well clear of the stack).
pub(super) const GUEST_STRING_VA: u64 = 0x3000;

/// The logical width/height of every top-level fixture surface.
pub(super) const SURFACE_WIDTH: i32 = 200;
pub(super) const SURFACE_HEIGHT: i32 = 100;

/// `GetSysColor(COLOR_BTNFACE)` — the released push-button face
/// (`controls::COLOR_BTNFACE`).
pub(super) const BTNFACE: u32 = 0x00F0_F0F0;
/// `controls::COLOR_BTNFACE_PRESSED` — the pressed face.
pub(super) const BTNFACE_PRESSED: u32 = 0x00D8_D8D8;
/// `GetSysColor(COLOR_BTNSHADOW)` — the 1 px control border.
pub(super) const BTNSHADOW: u32 = 0x00A0_A0A0;
/// `GetSysColor(COLOR_WINDOW)` — the EDIT / LISTBOX background.
pub(super) const WINDOW_WHITE: u32 = 0x00FF_FFFF;
/// `GetSysColor(COLOR_HIGHLIGHT)` — the selected row's fill.
pub(super) const HIGHLIGHT: u32 = 0x0000_78D7;
/// `COLOR_WINDOWTEXT` / `COLOR_BTNTEXT` — the glyph ink colour the painters
/// pass as `0`.
pub(super) const INK: u32 = 0x0000_0000;
/// A colour no control painter emits. Written into the surface by
/// [`sentinel`] so "which pixels did the repaint overwrite?" is answerable.
pub(super) const SENTINEL: u32 = 0x00FF_00FF;

/// Minimal engine: guest pages (for the item-string writes) plus a stack with
/// a valid return address — every handler ends in `return_from_win64_api`,
/// which pops from it.
pub(super) fn test_engine() -> IcedCpu {
    let mut cpu = IcedCpu::open_x86_64();
    cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
        .expect("map test memory");
    cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
        .expect("map test stack");
    cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
        .expect("write return address");
    cpu.write_rsp(STACK_TOP).ok();
    cpu
}

/// The crate's shared test state (the bump-heap-seeded variant).
pub(super) fn test_state() -> WinApiState {
    winapi_state_default_with_bump_heap()
}

/// A window's placement in its parent: `(x, y, width, height)`. Bundled so the
/// fixture helpers stay under clippy's `too_many_arguments` limit — the same
/// reason the production paint signatures bundle their parameters.
pub(super) type Placement = (i32, i32, i32, i32);

/// A visible 200×100 top-level window plus a visible child control of the
/// built-in class `class_name`, at `(x, y)` with the given extent and creation
/// text. Returns `(top, control)`.
///
/// The pair goes through `create_window_record`, so the control's
/// `control_kind` is resolved from the class name exactly as
/// `CreateWindowExW` resolves it, and a `WS_VISIBLE` control starts life
/// invalidated (as Windows paints a shown control).
pub(super) fn push_control(
    state: &mut WinApiState,
    class_name: &str,
    at: Placement,
    text: &str,
) -> (u64, u64) {
    push_control_with_style(state, class_name, 0, at, text)
}

/// Like [`push_control`], plus extra `dwStyle` bits — for the fixtures whose
/// paint depends on a creation style (a multiline EDIT's `ES_MULTILINE`, the
/// status bar's `CCS_BOTTOM`), which `push_control` cannot express.
pub(super) fn push_control_with_style(
    state: &mut WinApiState,
    class_name: &str,
    style: u32,
    at: Placement,
    text: &str,
) -> (u64, u64) {
    let top = create(
        state,
        crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
        crate::user32::WS_VISIBLE,
        0,
        (0, 0, SURFACE_WIDTH, SURFACE_HEIGHT),
        String::new(),
    );
    let child = create(
        state,
        crate::user32::WindowClassIdentifier::Name(class_name.to_owned()),
        crate::user32::WS_CHILD | crate::user32::WS_VISIBLE | style,
        top,
        at,
        text.to_owned(),
    );
    (top, child)
}

/// A visible 120×60 multiline EDIT at (10, 10) — the row-band fixture (three
/// 16 px rows fit the client exactly).
pub(super) fn push_multiline_edit(state: &mut WinApiState, text: &str) -> (u64, u64) {
    push_control_with_style(
        state,
        "EDIT",
        crate::user32::controls::ES_MULTILINE,
        (10, 10, 120, 60),
        text,
    )
}

/// One `create_window_record` call with the shared fixture shape (ANSI
/// windows — the item-string tests write ANSI guest strings).
fn create(
    state: &mut WinApiState,
    class_identifier: crate::user32::WindowClassIdentifier,
    style: u32,
    parent_handle: u64,
    at: Placement,
    title: String,
) -> u64 {
    let (x, y, width, height) = at;
    let (hwnd, _, _) = crate::user32::create_window_record(
        state,
        crate::user32::CreateWindowRequest {
            class_identifier,
            title,
            style,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x,
            y,
            width,
            height,
        },
        false,
    )
    .expect("window record created");
    assert_ne!(hwnd, 0, "create_window_record must allocate a handle");
    hwnd
}

/// Dispatch one control message and return its result, panicking on a guest
/// callback bridge (every message the tests drive is pure host-side work, so
/// a bridge means the fixture is wired wrong).
pub(super) fn send(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    wparam: u64,
    lparam: u64,
) -> u64 {
    crate::user32::controls::dispatch_control_proc(engine, state, hwnd, message, wparam, lparam)
        .unwrap_or_else(|error| panic!("message {message:#x} bridged a guest callback: {error:?}"))
        .unwrap_or_else(|| panic!("message {message:#x} was unhandled"))
}

/// `WM_PAINT` a control through the production dispatch, then flush the
/// deferred publish so [`published`] sees the result.
pub(super) fn paint(engine: &mut dyn CpuEngine, state: &mut WinApiState, hwnd: u64) {
    send(engine, state, hwnd, crate::user32::WM_PAINT, 0, 0);
    state.present().drain_pending_publishes();
}

/// The most recently published frame of `top`'s surface.
pub(super) fn published(state: &mut WinApiState, top: u64) -> SurfaceFrame {
    state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("a painted control publishes its ancestor surface")
        .clone()
}

/// The colour at `(x, y)` of `top`'s LIVE surface buffer.
///
/// [`published`] is the right oracle for "what did the guest see", but a paint
/// that is *skipped* publishes nothing — so the last published frame is stale
/// and cannot witness the absence of a write. For those tests the live buffer
/// is the only truthful observer.
pub(super) fn surface_pixel(state: &mut WinApiState, top: u64, x: i32, y: i32) -> u32 {
    let hwnd = crate::handles::Hwnd::from(top);
    let surface = state
        .present()
        .surfaces
        .get(&hwnd)
        .expect("the surface exists after the first paint");
    let pitch = usize::try_from(surface.stride).unwrap_or(0);
    let idx = usize::try_from(y.max(0))
        .unwrap_or(0)
        .saturating_mul(pitch)
        .saturating_add(usize::try_from(x.max(0)).unwrap_or(0));
    *surface
        .pixels
        .get(idx)
        .unwrap_or_else(|| panic!("pixel ({x},{y}) is outside the live surface"))
}

/// The colour at surface pixel `(x, y)`, panicking when the pixel is off the
/// frame (a mis-sized fixture must fail loudly, not read `None`).
pub(super) fn frame_pixel(frame: &SurfaceFrame, x: i32, y: i32) -> u32 {
    frame
        .pixel(
            u32::try_from(x).unwrap_or(u32::MAX),
            u32::try_from(y).unwrap_or(u32::MAX),
        )
        .unwrap_or_else(|| {
            panic!(
                "pixel ({x},{y}) is outside the {}x{} frame",
                frame.width, frame.height
            )
        })
}

/// Every colour in the surface rect `rect`, row by row (the callers index it
/// by `(row - rect.top, col - rect.left)`).
pub(super) fn frame_rect_pixels(frame: &SurfaceFrame, rect: IRect) -> Vec<Vec<u32>> {
    (rect.top..rect.bottom)
        .map(|y| {
            (rect.left..rect.right)
                .map(|x| frame_pixel(frame, x, y))
                .collect()
        })
        .collect()
}

/// How many pixels in `rect` are `color`.
pub(super) fn count_color(frame: &SurfaceFrame, rect: IRect, color: u32) -> usize {
    frame_rect_pixels(frame, rect)
        .iter()
        .flatten()
        .filter(|&&p| p == color)
        .count()
}

/// How many pixels in `rect` are NOT exactly `fill`.
///
/// The glyph rasterizer blends by coverage alpha, so one glyph's pixels span
/// the whole range from the fill colour to [`INK`] — there is no single "text
/// colour" to count. "Differs from the fill" is the AA-robust way to ask
/// "did anything get drawn here", and it is what every text assertion in this
/// tree uses.
pub(super) fn count_off_fill(frame: &SurfaceFrame, rect: IRect, fill: u32) -> usize {
    frame_rect_pixels(frame, rect)
        .iter()
        .flatten()
        .filter(|&&p| p != fill)
        .count()
}

/// Whether a column of the frame is uniformly `color` over `rows` — the shape
/// of a 1 px border/groove line, which AA never produces.
pub(super) fn column_is_uniform(
    frame: &SurfaceFrame,
    x: i32,
    rows: std::ops::Range<i32>,
    color: u32,
) -> bool {
    rows.clone().all(|y| frame_pixel(frame, x, y) == color)
}

/// Paint [`SENTINEL`] straight into `top`'s live surface over `rect`.
///
/// The repaint this sets up then answers a question a before/after frame
/// comparison cannot: the sentinel colour is in NO control palette, so every
/// pixel still magenta after a repaint was provably outside the repaint's
/// write scope. The write bypasses `mark_dirty` on purpose — it models foreign
/// content in the control's rect, not a guest write.
pub(super) fn sentinel(state: &mut WinApiState, top: u64, rect: IRect) {
    let hwnd = crate::handles::Hwnd::from(top);
    let needed = {
        let present = state.present();
        let surface = present
            .surfaces
            .get(&hwnd)
            .expect("the surface exists after the first paint");
        let stride = usize::try_from(surface.stride).unwrap_or(0);
        stride.saturating_mul(usize::try_from(surface.height).unwrap_or(0))
    };
    let surface = state
        .present()
        .surfaces
        .get_mut(&hwnd)
        .expect("the surface exists after the first paint");
    // A publish MOVES the buffer into the published frame, so the live
    // surface is empty here; re-establish it at the right pitch. The next
    // `ensure_surface` hand-back is skipped exactly because it is no longer
    // empty — which is what keeps the sentinel in place as the repaint base.
    if surface.pixels.len() != needed {
        surface.pixels.resize(needed, 0);
    }
    let pitch = usize::try_from(surface.stride).unwrap_or(0);
    let height = usize::try_from(surface.height).unwrap_or(0);
    for y in usize::try_from(rect.top.max(0)).unwrap_or(0)
        ..usize::try_from(rect.bottom.max(0)).unwrap_or(0).min(height)
    {
        let start = y
            .saturating_mul(pitch)
            .saturating_add(usize::try_from(rect.left.max(0)).unwrap_or(0));
        let end = start
            .saturating_add(usize::try_from((rect.right - rect.left).max(0)).unwrap_or(0))
            .min(surface.pixels.len());
        if let Some(row) = surface.pixels.get_mut(start..end) {
            for pixel in row {
                *pixel = SENTINEL;
            }
        }
    }
}

/// The resolved line height of a control's stored font (falling back to the
/// 16 px system default) — the row pitch every paint/scroll path agrees on.
pub(super) fn line_height(state: &mut WinApiState, hwnd: u64) -> i32 {
    state.with_font_engine(|state, font_engine| {
        crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine)
            .map_or(16, |(_key, resolved)| resolved.line_height())
    })
}

/// Write a NUL-terminated ANSI guest string at [`GUEST_STRING_VA`] and return
/// its address (the `LB_ADDSTRING` / `CB_ADDSTRING` input).
pub(super) fn guest_ansi(engine: &mut dyn CpuEngine, text: &str) -> u64 {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    engine
        .mem_write(GUEST_STRING_VA, &bytes)
        .expect("write guest ANSI string");
    GUEST_STRING_VA
}

/// Write a NUL-terminated UTF-16LE guest string at [`GUEST_STRING_VA`] and
/// return its address (the `SB_SETTEXTW` input).
pub(super) fn guest_utf16(engine: &mut dyn CpuEngine, text: &str) -> u64 {
    let mut bytes: Vec<u8> = Vec::new();
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine
        .mem_write(GUEST_STRING_VA, &bytes)
        .expect("write guest UTF-16 string");
    GUEST_STRING_VA
}

/// Write an `i32` array at [`GUEST_STRING_VA`] and return its address (the
/// `SB_SETPARTS` part-widths input).
pub(super) fn guest_i32_array(engine: &mut dyn CpuEngine, values: &[i32]) -> u64 {
    let mut bytes: Vec<u8> = Vec::new();
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    engine
        .mem_write(GUEST_STRING_VA, &bytes)
        .expect("write guest i32 array");
    GUEST_STRING_VA
}

/// The pending rect-scope invalidation of a BUTTON/STATIC/LISTBOX control.
pub(super) fn pending_label_invalid(
    state: &WinApiState,
    hwnd: u64,
) -> crate::user32::controls::LabelInvalidation {
    let control = state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
        .expect("the control state is seeded by its first message");
    match control {
        crate::user32::controls::ControlState::Button { invalidation, .. }
        | crate::user32::controls::ControlState::Static { invalidation }
        | crate::user32::controls::ControlState::ListBox { invalidation, .. } => *invalidation,
        other => panic!("not a rect-scope control: {other:?}"),
    }
}

/// Force a control's pending rect scope (the erase target `paint_control`
/// computes from). Seeding it directly is the honest way to test the SCOPE
/// machinery: the alternative — driving a real mutation — only ever produces
/// the rects `button_invalidate_pressed` / the LISTBOX row marks happen to
/// compute, so the "arbitrary pending rect" case stays uncovered.
pub(super) fn set_pending_label_invalid(
    state: &mut WinApiState,
    hwnd: u64,
    invalidation: crate::user32::controls::LabelInvalidation,
) {
    let control = state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| crate::user32::controls::ControlClassKind::Static.new_state());
    match control {
        crate::user32::controls::ControlState::Button {
            invalidation: slot, ..
        }
        | crate::user32::controls::ControlState::Static { invalidation: slot }
        | crate::user32::controls::ControlState::ListBox {
            invalidation: slot, ..
        } => {
            *slot = invalidation;
        }
        other => panic!("not a rect-scope control: {other:?}"),
    }
}
