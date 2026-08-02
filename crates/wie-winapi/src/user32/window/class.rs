//! Capture/mouse tracking, the window/class long-pointer accessors, and the
//! window-lookup helpers (split from the former `window.rs`).

use crate::user32::{
    Context, HandlerContext, Result, WinApiHandlerResult, WinApiState, WindowRecord,
    get_window_long_ptr_value, is_known_window, set_window_long_ptr_value, window_long_ptr_index,
};

/// Handles `USER32.dll!SetWindowLongPtrW`.
pub fn handle_set_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowLongPtrW")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowLongPtrW")?;

    let new_value = engine
        .read_r8()
        .context("failed to read R8 for SetWindowLongPtrW")?;

    let previous_value = set_window_long_ptr_value(
        window_handle,
        index_raw,
        new_value,
        state,
        "SetWindowLongPtrW",
    )?;

    let return_address = engine
        .return_from_win64_api(previous_value)
        .context("failed to return from SetWindowLongPtrW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_value,
    })
}

/// Handles `USER32.dll!SetCapture`.
pub fn handle_set_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetCapture")?;

    let previous_window = state.window_state().capture_window_handle;

    // SetCapture(NULL) releases; real windows (including child controls, whose
    // WndProc captures implicitly while pressed) become the capture owner.
    if window_handle == 0 || is_known_window(state, window_handle) {
        state.window_state().capture_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    let return_address = engine
        .return_from_win64_api(previous_window.as_u64())
        .context("failed to return from SetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window.as_u64(),
    })
}
/// Handles `USER32.dll!GetCapture`.
pub fn handle_get_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().capture_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ReleaseCapture`.
pub fn handle_release_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    state.window_state().capture_window_handle = crate::handles::Hwnd::NULL;

    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ReleaseCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!GetWindowLongPtrA`.
pub fn handle_get_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowLongPtrA")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowLongPtrA")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrA")?;

    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from GetWindowLongPtrA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
/// Handles `USER32.dll!GetWindowLongPtrW`.
pub fn handle_get_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowLongPtrW")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowLongPtrW")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrW")?;

    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from GetWindowLongPtrW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
/// Handles `USER32.dll!SetWindowLongPtrA`.
pub fn handle_set_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowLongPtrA")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowLongPtrA")?;

    let new_value = engine
        .read_r8()
        .context("failed to read R8 for SetWindowLongPtrA")?;

    let previous_value = set_window_long_ptr_value(
        window_handle,
        index_raw,
        new_value,
        state,
        "SetWindowLongPtrA",
    )?;

    let return_address = engine
        .return_from_win64_api(previous_value)
        .context("failed to return from SetWindowLongPtrA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_value,
    })
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
    handle_get_class_long_ptr(ctx, "GetClassLongPtrA")
}

/// Handles `USER32.dll!GetClassLongPtrW`.
pub fn handle_get_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_long_ptr(ctx, "GetClassLongPtrW")
}

fn handle_get_class_long_ptr(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let index_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

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

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!SetClassLongPtrA`.
pub fn handle_set_class_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr(ctx, "SetClassLongPtrA")
}

/// Handles `USER32.dll!SetClassLongPtrW`.
pub fn handle_set_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr(ctx, "SetClassLongPtrW")
}

fn handle_set_class_long_ptr(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let index_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let new_value = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let atom = window_class_atom(state, window_handle);

    let return_value = if atom == 0 {
        0
    } else {
        let index = window_long_ptr_index(index_raw, api_name)?;
        set_class_long_ptr_value(state, atom, index, new_value)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
