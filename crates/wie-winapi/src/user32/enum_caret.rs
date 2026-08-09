//! USER32 enumeration + caret lane (soft-dispatch): `EnumWindows`,
//! `EnumChildWindows`, `FindWindowA/W`, the caret family (`CreateCaret`,
//! `SetCaretPos`, `GetCaretPos`, `ShowCaret`, `HideCaret`, `DestroyCaret`)
//! and the `DrawIcon*` no-ops.
//!
//! Routed from `dispatch_user32_extra` (user32/mod.rs) — the string-match
//! fallback the dense `WinApiId` table does not cover. All handlers here are
//! the "universal but partial" Tier-2 surface; none are hot-path APIs.

use std::sync::Mutex;

use super::{
    Context, GuestCallbackRequest, HandlerContext, Result, WinApiControlSignal,
    WinApiHandlerResult, WindowClassIdentifier, is_known_window, low_i32, read_guest_ansi_lossy,
    read_guest_utf16_lossy, read_window_class_identifier_a, read_window_class_identifier_w,
    with_typed_write,
};
use crate::guest_layout::WinPoint;
use crate::handles::Hwnd;
use crate::{OuterReturn, WinApiState};

/// Per-window caret size stored by `CreateCaret(hWnd, hBitmap, nWidth,
/// nHeight)`.
///
/// Real Windows owns one caret per thread (a thread-relative object), but the
/// API keys the width/height on the window; keeping the sizes keyed by hwnd
/// mirrors the API shape without touching `WindowRecord` (the record carries
/// no caret field, so the static-store approach is used — the `strtok` SAVE
/// pattern, process-global like the real single caret). `HashMap::new` is not
/// `const`, so the store is a small pair `Vec` (the `PICK_MOUNTS` shape). A
/// poisoned lock fail-closes: the handler reports the call as failed instead
/// of unwinding.
static CARET_SIZES: Mutex<Vec<(u64, (u64, u64))>> = Mutex::new(Vec::new());

/// The thread-global caret position stored by `SetCaretPos`, read back by
/// `GetCaretPos`. `None` = never set (GetCaretPos then returns (0, 0)).
static CARET_POSITION: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Dispatches the enum/caret/drawicon lane of `dispatch_user32_extra`.
pub(crate) fn dispatch_enum_caret(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "enumwindows" => Ok(Some(handle_enum_windows(ctx)?)),
        "enumchildwindows" => Ok(Some(handle_enum_child_windows(ctx)?)),
        "findwindowa" => Ok(Some(handle_find_window_a(ctx)?)),
        "findwindoww" => Ok(Some(handle_find_window_w(ctx)?)),
        "createcaret" => Ok(Some(handle_create_caret(ctx)?)),
        "setcaretpos" => Ok(Some(handle_set_caret_pos(ctx)?)),
        "getcaretpos" => Ok(Some(handle_get_caret_pos(ctx)?)),
        "showcaret" => Ok(Some(handle_show_caret(ctx)?)),
        "hidecaret" => Ok(Some(handle_hide_caret(ctx)?)),
        "destroycaret" => Ok(Some(handle_destroy_caret(ctx)?)),
        // mingw's user32 import lib exports the plain name "DrawIcon" (not
        // DrawIconA/DrawIconW) — cover both spellings.
        "drawicon" | "drawicona" | "drawiconw" => Ok(Some(handle_draw_icon(ctx)?)),
        "drawiconex" => Ok(Some(handle_draw_icon_ex(ctx)?)),
        _ => Ok(None),
    }
}

/// Bridge one guest `WNDENUMPROC` call through the runtime's WndProc callback
/// machinery ([`WinApiControlSignal::GuestCallbackRequested`]).
///
/// The guest-callback bridge is WndProc ABI (RCX = hwnd, RDX = message `u32`,
/// R8 = wParam, R9 = lParam). An `EnumWindowsProc` reads (RCX = hwnd, RDX =
/// lParam), so the lParam rides in the u32 message slot — only its low 32 bits
/// cross the bridge (a documented limitation: enumeration lParams are almost
/// always 0 or small integers; a pointer context would truncate).
///
/// The bridge is one-shot: the runtime completes the outer API as soon as the
/// callback returns, so one `EnumWindows` stop can invoke the callback for at
/// most one window (the first match). Full N-window enumeration would need a
/// runtime continuation hook that does not exist; the callback's BOOL becomes
/// the API's return value (Passthrough), so a callback returning 0 surfaces as
/// `EnumWindows` returning 0.
fn emit_enum_callback(
    callback_address: u64,
    window_handle: u64,
    long_parameter: u64,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let low_long_parameter = u32::try_from(long_parameter & u64::from(u32::MAX)).unwrap_or(0);

    tracing::debug!(
        target: "wiegui",
        api = api_name,
        callback = callback_address,
        hwnd = window_handle,
        "enumeration callback"
    );

    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address,
            window_handle,
            message: low_long_parameter,
            word_parameter: 0,
            long_parameter: 0,
            unicode: false,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}

/// The first window in the record list whose parent matches `parent_handle`.
///
/// Enumeration order is record (creation) order — real Windows enumerates in
/// Z-order, but a stable creation order is the documented approximation.
fn first_window_with_parent(state: &mut WinApiState, parent_handle: u64) -> u64 {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| window.parent_handle == Hwnd::from(parent_handle))
        .map_or(0, |window| window.handle.as_u64())
}

/// Handles `USER32.dll!EnumWindows`.
///
/// Win64 ABI: `rcx` = `lpEnumFunc`, `rdx` = `lParam`. Enumerates the first
/// top-level window record (a record with no parent/owner) through the guest
/// callback; a NULL callback or an empty window list completes the
/// enumeration trivially. See [`emit_enum_callback`] for the one-shot
/// limitation.
pub(crate) fn handle_enum_windows(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let callback = engine
        .read_rcx()
        .context("failed to read RCX for EnumWindows")?;
    let long_parameter = engine
        .read_rdx()
        .context("failed to read RDX for EnumWindows")?;

    if callback == 0 {
        return ctx.finish(0);
    }

    let window_handle = first_window_with_parent(state, 0);
    if window_handle == 0 {
        // No top-level windows: enumeration trivially completed.
        return ctx.finish(1);
    }

    emit_enum_callback(callback, window_handle, long_parameter, "EnumWindows")
}

/// Handles `USER32.dll!EnumChildWindows`.
///
/// Win64 ABI: `rcx` = `hWndParent`, `rdx` = `lpEnumFunc`, `r8` = `lParam`.
/// A NULL parent is equivalent to `EnumWindows` (top-level windows only).
pub(crate) fn handle_enum_child_windows(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_handle = engine
        .read_rcx()
        .context("failed to read RCX for EnumChildWindows")?;
    let callback = engine
        .read_rdx()
        .context("failed to read RDX for EnumChildWindows")?;
    let long_parameter = engine
        .read_r8()
        .context("failed to read R8 for EnumChildWindows")?;

    if callback == 0 {
        return ctx.finish(0);
    }

    let window_handle = first_window_with_parent(state, parent_handle);
    if window_handle == 0 {
        return ctx.finish(1);
    }

    emit_enum_callback(callback, window_handle, long_parameter, "EnumChildWindows")
}

/// Handles `USER32.dll!FindWindowA`.
pub(crate) fn handle_find_window_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let class_name_va = engine
        .read_rcx()
        .context("failed to read RCX for FindWindowA")?;
    let window_name_va = engine
        .read_rdx()
        .context("failed to read RDX for FindWindowA")?;
    let class_identifier = read_window_class_identifier_a(engine, class_name_va)?;
    let window_name = read_guest_ansi_lossy(engine, window_name_va, 256)
        .context("failed to read FindWindowA window name")?;
    let hwnd = find_window_by_class_and_title(ctx.state, &class_identifier, &window_name);
    ctx.finish(hwnd)
}

/// Handles `USER32.dll!FindWindowW`.
pub(crate) fn handle_find_window_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let class_name_va = engine
        .read_rcx()
        .context("failed to read RCX for FindWindowW")?;
    let window_name_va = engine
        .read_rdx()
        .context("failed to read RDX for FindWindowW")?;
    let class_identifier = read_window_class_identifier_w(engine, class_name_va)?;
    let window_name = read_guest_utf16_lossy(engine, window_name_va, 256)
        .context("failed to read FindWindowW window name")?;
    let hwnd = find_window_by_class_and_title(ctx.state, &class_identifier, &window_name);
    ctx.finish(hwnd)
}

/// The first top-level window whose class identifier and (optional) title
/// both match — `FindWindow`'s contract. Both matchers are case-insensitive
/// (MSDN), and an empty name (NULL argument) matches any title.
fn find_window_by_class_and_title(
    state: &mut WinApiState,
    class_identifier: &WindowClassIdentifier,
    window_name: &str,
) -> u64 {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| {
            window.parent_handle == Hwnd::NULL
                && window_class_matches(window, class_identifier)
                && (window_name.is_empty() || window.title.eq_ignore_ascii_case(window_name))
        })
        .map_or(0, |window| window.handle.as_u64())
}

fn window_class_matches(window: &super::WindowRecord, identifier: &WindowClassIdentifier) -> bool {
    match identifier {
        WindowClassIdentifier::Atom(atom) => window.class_atom == *atom,
        WindowClassIdentifier::Name(name) => window.class_name.eq_ignore_ascii_case(name),
    }
}

/// Handles `USER32.dll!CreateCaret`.
///
/// Win64 ABI: `rcx` = `hWnd`, `rdx` = `hBitmap`, `r8` = `nWidth`, `r9` =
/// `nHeight`. Stores the requested width/height for the window; the caret is
/// never rendered (KISS — the EDIT control paints its own caret).
pub(crate) fn handle_create_caret(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for CreateCaret")?;
    let _bitmap_handle = engine
        .read_rdx()
        .context("failed to read RDX for CreateCaret")?;
    let width = engine
        .read_r8()
        .context("failed to read R8 for CreateCaret")?;
    let height = engine
        .read_r9()
        .context("failed to read R9 for CreateCaret")?;

    if !is_known_window(state, window_handle) {
        return ctx.finish(0);
    }

    let mut sizes = CARET_SIZES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = sizes.iter_mut().find(|(hwnd, _)| *hwnd == window_handle) {
        entry.1 = (width, height);
    } else {
        sizes.push((window_handle, (width, height)));
    }

    ctx.finish(1)
}

/// Handles `USER32.dll!SetCaretPos`.
///
/// Win64 ABI: `rcx` = `X`, `rdx` = `Y` (sign-extended `int`s).
pub(crate) fn handle_set_caret_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x_raw = engine
        .read_rcx()
        .context("failed to read RCX for SetCaretPos")?;
    let y_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetCaretPos")?;
    let x = low_i32(x_raw, "SetCaretPos x")?;
    let y = low_i32(y_raw, "SetCaretPos y")?;

    *CARET_POSITION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((x, y));

    ctx.finish(1)
}

/// Handles `USER32.dll!GetCaretPos`.
///
/// Win64 ABI: `rcx` = `lpPoint`. Writes the stored caret position (0, 0 when
/// never set) as a `POINT` and returns TRUE.
pub(crate) fn handle_get_caret_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let point_va = engine
        .read_rcx()
        .context("failed to read RCX for GetCaretPos")?;

    let (x, y) = CARET_POSITION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or((0, 0));

    if point_va != 0 {
        with_typed_write::<WinPoint, _, _>(engine, point_va, |point| {
            point.x = x;
            point.y = y;
            Ok(())
        })
        .context("failed to write GetCaretPos POINT")?;
    }

    ctx.finish(1)
}

/// Handles `USER32.dll!ShowCaret`.
pub(crate) fn handle_show_caret(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _window_handle = ctx
        .engine
        .read_rcx()
        .context("failed to read RCX for ShowCaret")?;
    ctx.finish(1)
}

/// Handles `USER32.dll!HideCaret`.
pub(crate) fn handle_hide_caret(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _window_handle = ctx
        .engine
        .read_rcx()
        .context("failed to read RCX for HideCaret")?;
    ctx.finish(1)
}

/// Handles `USER32.dll!DestroyCaret`.
///
/// Real `DestroyCaret` takes no argument; the micro passes the caret's hwnd
/// in RCX anyway, so the stored size keyed by that handle (if any) is
/// dropped. The position is not cleared (Windows keeps it thread-global).
pub(crate) fn handle_destroy_caret(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let window_handle = ctx
        .engine
        .read_rcx()
        .context("failed to read RCX for DestroyCaret")?;

    let mut sizes = CARET_SIZES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sizes.retain(|(hwnd, _)| *hwnd != window_handle);

    ctx.finish(1)
}

/// Handles `USER32.dll!DrawIconA` and `DrawIconW`.
///
/// Win64 ABI: `rcx` = `hDC`, `rdx` = `X`, `r8` = `Y`, `r9` = `hIcon`. Icons
/// are decorative (nothing is rendered) — the call always succeeds.
pub(crate) fn handle_draw_icon(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _dc = ctx
        .engine
        .read_rcx()
        .context("failed to read RCX for DrawIcon")?;
    let _x = ctx
        .engine
        .read_rdx()
        .context("failed to read RDX for DrawIcon")?;
    let _y = ctx
        .engine
        .read_r8()
        .context("failed to read R8 for DrawIcon")?;
    let _icon = ctx
        .engine
        .read_r9()
        .context("failed to read R9 for DrawIcon")?;
    ctx.finish(1)
}

/// Handles `USER32.dll!DrawIconEx` — no-op like the plain `DrawIcon` (nine
/// arguments, none of them consumed).
pub(crate) fn handle_draw_icon_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _dc = ctx
        .engine
        .read_rcx()
        .context("failed to read RCX for DrawIconEx")?;
    let _x = ctx
        .engine
        .read_rdx()
        .context("failed to read RDX for DrawIconEx")?;
    let _y = ctx
        .engine
        .read_r8()
        .context("failed to read R8 for DrawIconEx")?;
    let _icon = ctx
        .engine
        .read_r9()
        .context("failed to read R9 for DrawIconEx")?;
    ctx.finish(1)
}
