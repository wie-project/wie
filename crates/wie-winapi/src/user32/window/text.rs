//! Window text and font state: `SetWindowTextA/W`, `GetWindowTextA/W`, the
//! length queries, and the shared caption/font helpers (split from the former
//! `window.rs`).

use super::class::{find_window, find_window_mut};
use crate::state::WindowFlags;
use crate::user32::{
    Context, FAKE_WINDOW_HANDLE, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    is_known_window, read_arg_string, write_out_string,
};

/// Handles `USER32.dll!SetWindowTextA`.
pub fn handle_set_window_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_window_text_impl(ctx, false, "SetWindowTextA")
}
/// Handles `USER32.dll!SetWindowTextW`.
pub fn handle_set_window_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_window_text_impl(ctx, true, "SetWindowTextW")
}

/// Shared `SetWindowTextA/W` implementation (Win64 ABI: `rcx` = hwnd,
/// `rdx` = text).
///
/// `SetWindowText(hwnd, NULL)` clears the text (documented Win32 semantics);
/// RNotepad's FileNew/DoOpenFile clear the EDIT this way. A NULL pointer is
/// not a failure — it means "empty string".
fn handle_set_window_text_impl(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let text_va = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let known = window_handle == FAKE_WINDOW_HANDLE || is_known_window(state, window_handle);
    let success = known;

    if success {
        let text = if text_va == 0 {
            String::new()
        } else {
            read_arg_string(engine, text_va, wide)
                .with_context(|| format!("failed to read {api_name} text"))?
        };
        if window_handle == FAKE_WINDOW_HANDLE {
            state.window_state().window_title = text;
        } else if let Some(window) = find_window_mut(state, window_handle) {
            // Controls repaint with their new caption; other windows get the
            // title updated.
            if window.control_kind.is_some() {
                // A label control's old caption must survive the replacement:
                // the text-change invalidation measures both captions so the
                // next paint erases the previous glyphs (same as the WM_SETTEXT
                // dispatch arm).
                let old_text = if matches!(
                    window.control_kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    window.control_text.clone()
                } else {
                    String::new()
                };
                let kind = window.control_kind;
                window.control_text = text.clone();
                window.invalidated = true;
                // Real Windows clears an EDIT's undo buffer when the program
                // sets the text — WM_UNDO must not revert past it (the
                // WM_SETTEXT dispatch arm does the same).
                crate::user32::controls::edit_clear_undo_buffer(state, window_handle);
                // An EDIT's caret+selection move to the END of the text set
                // programmatically (real Windows): the caret lands after the
                // last character, so typing appends to a prefilled field.
                // `-1` is the EM_SETSEL "end of text" convention; the FileNew
                // `SetWindowText(hEdit, NULL)` empty case resolves to (0, 0),
                // and the guest's Ln/Col status refresh reads the start.
                if kind == Some(crate::user32::controls::ControlClassKind::Edit) {
                    crate::user32::controls::edit_set_selection(state, window_handle, -1, -1);
                    crate::user32::controls::edit_reset_invalid_rows(state, window_handle);
                }
                if matches!(
                    kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    crate::user32::controls::label_invalidate_text_change(
                        state,
                        window_handle,
                        &old_text,
                        &text,
                    );
                }
            } else {
                window.title = text;
            }
        }
        // The visible change: bump the owning top-level's content revision so
        // the idle reconcile republishes the surface even if the next repaint
        // cycle is skipped (the pull-based repaint latch).
        crate::present::PresentState::request_paint(state, window_handle);
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles `USER32.dll!GetWindowTextA`.
pub fn handle_get_window_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_window_text_impl(ctx, false, "GetWindowTextA")
}
/// Handles `USER32.dll!GetWindowTextW`.
pub fn handle_get_window_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_window_text_impl(ctx, true, "GetWindowTextW")
}

/// Shared `GetWindowTextA/W` implementation (Win64 ABI: `rcx` = hwnd,
/// `rdx` = buffer, `r8` = max characters). Returns the copied content count
/// (characters for A, units for W), excluding the NUL.
fn handle_get_window_text_impl(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let buffer_va = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let max_characters = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let text = resolve_window_text(state, window_handle);

    let return_value = if text.is_empty() {
        0
    } else {
        write_out_string(engine, buffer_va, max_characters, &text, wide)?
    };

    ctx.finish(return_value)
}

/// Handles `USER32.dll!GetWindowTextLengthW`.
pub fn handle_get_window_text_length_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextLengthW")?;

    let text = resolve_window_text(state, window_handle);

    // Count of UTF-16 units — exactly what GetWindowTextW would copy,
    // excluding the terminating NUL (mirrors write_guest_utf16_c_string's
    // length semantics). Empty/unknown text resolves to "" → 0.
    let length = u64::try_from(text.encode_utf16().count())
        .context("window text length does not fit u64")?;

    ctx.finish(length)
}

/// Handles `USER32.dll!GetWindowTextLengthA`.
pub fn handle_get_window_text_length_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextLengthA")?;

    let text = resolve_window_text(state, window_handle);

    // Count of CP1252 characters — what Windows GetWindowTextLengthA reports
    // (the ACP is 1252, one byte per char). `encode_cp1252` emits one byte
    // per char with '?' (0x3F) for unmappables — the same bytes
    // write_guest_ansi_c_string writes, so the ANSI byte count is the CP1252
    // char count (mirrors the A write path's length semantics).
    // Empty/unknown text resolves to "" → 0.
    let length = u64::try_from(crate::guest_string::encode_cp1252(&text).len())
        .context("window text length does not fit u64")?;

    ctx.finish(length)
}

/// The text GetWindowTextA/W reports for a window: the control buffer for
/// built-in controls, the title otherwise (the legacy fake window's title
/// lives in `WindowState.window_title`).
fn resolve_window_text(state: &mut WinApiState, window_handle: u64) -> String {
    if window_handle == FAKE_WINDOW_HANDLE {
        return state.window_state().window_title.clone();
    }
    find_window(state, window_handle).map_or_else(String::new, |window| {
        if window.control_kind.is_some() {
            window.control_text.clone()
        } else {
            window.title.clone()
        }
    })
}

/// WM_SETFONT (DefWindowProc semantics): store `font_handle` on the window so
/// the paint paths draw its text with the guest-selected font, and mark the
/// window invalidated when `redraw` is non-zero (the next repaint cycle then
/// re-renders with the new font). Real Windows sends WM_ERASEBKGND before that
/// repaint, so a redraw also requests the pending erase — the same idiom
/// SetWindowPlacement and the resize path use. Callers return the WM_SETFONT
/// result (0).
///
/// Shared by the control dispatch (`dispatch_control_proc`), `DefWindowProc`,
/// and the no-WndProc `SendMessage` fallthrough so ANY window — control or
/// not — stores the font it is told to use.
pub(crate) fn set_window_font(state: &mut WinApiState, hwnd: u64, font_handle: u64, redraw: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.font_handle = crate::handles::Hfont::from(font_handle);
        if redraw != 0 {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }
    if redraw != 0 {
        // A redraw is a visible change: bump the content revision so the idle
        // reconcile republishes with the new font. An unknown window resolves
        // to nothing and is a silent no-op.
        crate::present::PresentState::request_paint(state, hwnd);
    }
}

/// WM_GETFONT (DefWindowProc semantics): the HFONT stored on the window, or 0
/// when never set (or the window is unknown).
#[must_use]
pub(crate) fn window_font(state: &mut WinApiState, hwnd: u64) -> u64 {
    find_window(state, hwnd).map_or(0, |window| window.font_handle.as_u64())
}
