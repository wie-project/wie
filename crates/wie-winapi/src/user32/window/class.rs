//! Capture/mouse tracking, the window/class long-pointer accessors, and the
//! window-lookup helpers (split from the former `window.rs`).

use crate::gdi32::{ArgReg, read_arg};
use crate::user32::{
    GWLP_WNDPROC, HandlerContext, Result, WinApiHandlerResult, WinApiState, WindowRecord,
    get_window_long_ptr_value, is_known_window, set_window_long_ptr_value, window_long_ptr_index,
};

/// Handles `USER32.dll!SetWindowLongPtrW`.
pub fn handle_set_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_window_long_ptr_impl(ctx, "SetWindowLongPtrW")
}

/// Handles `USER32.dll!SetWindowLongPtrA`.
pub fn handle_set_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_window_long_ptr_impl(ctx, "SetWindowLongPtrA")
}

/// Shared `SetWindowLongPtrA/W` implementation.
///
/// `GWLP_WNDPROC` additionally records the replaced value as the window's
/// `subclass_original_wndproc` (on the FIRST subclass) — for a built-in
/// control that is WIE's default-control-proc marker (0), which
/// `CallWindowProcW(hwnd, <marker>, …)` recognizes to run the host default
/// control dispatch when the guest subclass forwards a message it does not
/// handle (notepad's `EDIT_WndProc` pattern).
fn handle_set_window_long_ptr_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, api_name)?;

    let index_raw = read_arg(engine, ArgReg::Rdx, api_name)?;

    let new_value = read_arg(engine, ArgReg::R8, api_name)?;

    let previous_value =
        set_window_long_ptr_value(window_handle, index_raw, new_value, state, api_name)?;

    remember_subclass_original(state, window_handle, index_raw, previous_value);

    ctx.finish(previous_value)
}

/// When a guest replaces `GWLP_WNDPROC`, remember the proc it displaced the
/// first time (see the handler doc above). Re-subclassing and restoring keep
/// the original class default unchanged.
fn remember_subclass_original(
    state: &mut WinApiState,
    window_handle: u64,
    index_raw: u64,
    previous_value: u64,
) {
    if window_long_ptr_index(index_raw, "SetWindowLongPtr").ok() != Some(GWLP_WNDPROC) {
        return;
    }
    if let Some(window) = find_window_mut(state, window_handle)
        && window.subclass_original_wndproc == 0
    {
        window.subclass_original_wndproc = previous_value;
    }
}

/// Handles `USER32.dll!SetCapture`.
pub fn handle_set_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, "SetCapture")?;

    let previous_window = state.window_state().capture_window_handle;

    // SetCapture(NULL) releases; real windows (including child controls, whose
    // WndProc captures implicitly while pressed) become the capture owner.
    if window_handle == 0 || is_known_window(state, window_handle) {
        state.window_state().capture_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    ctx.finish(previous_window.as_u64())
}
/// Handles `USER32.dll!GetCapture`.
pub fn handle_get_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let return_value = state.window_state().capture_window_handle.as_u64();

    ctx.finish(return_value)
}
/// Handles `USER32.dll!ReleaseCapture`.
pub fn handle_release_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    state.window_state().capture_window_handle = crate::handles::Hwnd::NULL;

    let return_value = 1;

    ctx.finish(return_value)
}

/// Handles `USER32.dll!GetWindowLongPtrA`.
pub fn handle_get_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, "GetWindowLongPtrA")?;

    let index_raw = read_arg(engine, ArgReg::Rdx, "GetWindowLongPtrA")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrA")?;

    ctx.finish(value)
}
/// Handles `USER32.dll!GetWindowLongPtrW`.
pub fn handle_get_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, "GetWindowLongPtrW")?;

    let index_raw = read_arg(engine, ArgReg::Rdx, "GetWindowLongPtrW")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrW")?;

    ctx.finish(value)
}

pub(crate) fn find_window(state: &mut WinApiState, handle: u64) -> Option<&WindowRecord> {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(handle))
}

pub(crate) fn find_window_mut(state: &mut WinApiState, handle: u64) -> Option<&mut WindowRecord> {
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|window| window.handle == crate::handles::Hwnd::from(handle))
}

/// Handles `USER32.dll!GetClassLongPtrA`.
pub fn handle_get_class_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_long_ptr_impl(ctx, "GetClassLongPtrA")
}

/// Handles `USER32.dll!GetClassLongPtrW`.
pub fn handle_get_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_long_ptr_impl(ctx, "GetClassLongPtrW")
}

fn handle_get_class_long_ptr_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, api_name)?;

    let index_raw = read_arg(engine, ArgReg::Rdx, api_name)?;

    let atom = window_class_atom(state, window_handle);

    let return_value = if atom == 0 {
        0
    } else {
        let index = window_long_ptr_index(index_raw, api_name)?;
        state
            .window_state()
            .class_long_ptr_values
            .iter()
            .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
            .map_or(0, |(_, _, value)| *value)
    };

    ctx.finish(return_value)
}

/// Handles `USER32.dll!SetClassLongPtrA`.
pub fn handle_set_class_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr_impl(ctx, "SetClassLongPtrA")
}

/// Handles `USER32.dll!SetClassLongPtrW`.
pub fn handle_set_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr_impl(ctx, "SetClassLongPtrW")
}

fn handle_set_class_long_ptr_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, api_name)?;

    let index_raw = read_arg(engine, ArgReg::Rdx, api_name)?;

    let new_value = read_arg(engine, ArgReg::R8, api_name)?;

    let atom = window_class_atom(state, window_handle);

    let return_value = if atom == 0 {
        0
    } else {
        let index = window_long_ptr_index(index_raw, api_name)?;
        set_class_long_ptr_value(state, atom, index, new_value)
    };

    ctx.finish(return_value)
}

/// Resolve a window handle to its registered class atom (0 when unregistered).
fn window_class_atom(state: &mut WinApiState, window_handle: u64) -> u16 {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(window_handle))
        .map_or(0, |window| window.class_atom)
}

/// Store a class-long value; returns the previous value.
fn set_class_long_ptr_value(state: &mut WinApiState, atom: u16, index: i64, new_value: u64) -> u64 {
    let previous_value = state
        .window_state()
        .class_long_ptr_values
        .iter()
        .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
        .map_or(0, |(_, _, value)| *value);

    if let Some(entry) = state
        .window_state()
        .class_long_ptr_values
        .iter_mut()
        .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
    {
        entry.2 = new_value;
    } else {
        state
            .window_state()
            .class_long_ptr_values
            .push((atom, index, new_value));
    }

    previous_value
}
