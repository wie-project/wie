use super::{
    Context, FAKE_DESKTOP_WINDOW_HANDLE, FAKE_PROCESS_ID, FAKE_SYSTEM_COLOR_BRUSH_BASE,
    FAKE_THREAD_ID, FAKE_WINDOW_HANDLE, Result, WinApiHandlerResult, WinApiState, WindowRecord,
    checked_field_address, get_window_long_ptr_value, is_known_window, low_i32,
    read_guest_ansi_lossy, read_guest_i32, read_guest_u64, read_guest_utf16_lossy,
    set_window_long_ptr_value, window_client_size, write_ansi_window_text, write_guest_i32,
    write_guest_u32, write_wide_window_text, write_window_rect,
};

pub fn handle_get_window_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowRect")?;

    let rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowRect")?;

    let window = find_window(state, window_handle);
    let success = window.is_some() && rect_ptr != 0;

    if let Some(window) = window.filter(|_| rect_ptr != 0) {
        let right = window
            .x
            .checked_add(window.width)
            .context("GetWindowRect right coordinate overflow")?;

        let bottom = window
            .y
            .checked_add(window.height)
            .context("GetWindowRect bottom coordinate overflow")?;

        write_window_rect(engine, rect_ptr, window.x, window.y, right, bottom)
            .context("failed to write GetWindowRect RECT")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_dpi_for_window(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetDpiForWindow")?;

    // Standard 100% Windows DPI.
    let return_value = 96;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDpiForWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_adjust_window_rect_ex_for_dpi(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for AdjustWindowRectExForDpi")?;

    let _style = engine
        .read_rdx()
        .context("failed to read RDX for AdjustWindowRectExForDpi")?;

    let _has_menu = engine
        .read_r8()
        .context("failed to read R8 for AdjustWindowRectExForDpi")?;

    let _extended_style = engine
        .read_r9()
        .context("failed to read R9 for AdjustWindowRectExForDpi")?;

    // The fifth argument, dpi, is on the Win64 stack. For now the fake desktop
    // uses 96 DPI, so preserving the supplied client rectangle is sufficient.
    let return_value = u64::from(rect_ptr != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from AdjustWindowRectExForDpi")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_window_pos(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowPos")?;

    let _insert_after = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowPos")?;

    let _x = engine
        .read_r8()
        .context("failed to read R8 for SetWindowPos")?;

    let _y = engine
        .read_r9()
        .context("failed to read R9 for SetWindowPos")?;

    // Remaining Win64 arguments are width, height and flags on the stack.
    // For now the compatibility harness accepts the requested placement
    // without maintaining a full window manager.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowPos")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_window_long_ptr_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_is_window(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindow")?;

    let return_value = u64::from(window_handle == FAKE_WINDOW_HANDLE);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_is_window_visible(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowVisible")?;

    let return_value = u64::from(window_handle == FAKE_WINDOW_HANDLE);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowVisible")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_is_window_enabled(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowEnabled")?;

    let return_value = u64::from(window_handle == FAKE_WINDOW_HANDLE);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowEnabled")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_parent(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetParent")?;

    // The current fake top-level window has no parent.
    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetParent")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_active_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let return_value = state.window_state.active_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_foreground_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let return_value = state.window_state.foreground_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetForegroundWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_show_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ShowWindow")?;

    let show_command = engine
        .read_rdx()
        .context("failed to read RDX for ShowWindow")?;

    let previously_visible = state.window_state.window_visible;

    if window_handle == FAKE_WINDOW_HANDLE {
        // SW_HIDE is zero. Other commands make the window visible in the
        // current single-window model.
        state.window_state.window_visible = show_command != 0;
    }

    let return_value = u64::from(previously_visible);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ShowWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_enable_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for EnableWindow")?;

    let enable_raw = engine
        .read_rdx()
        .context("failed to read RDX for EnableWindow")?;

    let previously_disabled = !state.window_state.window_enabled;

    if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state.window_enabled = enable_raw != 0;
    }

    // EnableWindow returns nonzero when the window was previously disabled.
    let return_value = u64::from(previously_disabled);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EnableWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_foreground_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetForegroundWindow")?;

    let success = window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state.foreground_window_handle = window_handle;
        state.window_state.active_window_handle = window_handle;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetForegroundWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_active_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetActiveWindow")?;

    let previous_window = state.window_state.active_window_handle;

    if window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE {
        state.window_state.active_window_handle = window_handle;
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
    })
}
pub fn handle_set_focus(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetFocus")?;

    let previous_window = state.window_state.focus_window_handle;

    if window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE {
        state.window_state.focus_window_handle = window_handle;
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
    })
}
pub fn handle_get_focus(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let return_value = state.window_state.focus_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_capture(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetCapture")?;

    let previous_window = state.window_state.capture_window_handle;

    if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state.capture_window_handle = window_handle;
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
    })
}
pub fn handle_get_capture(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let return_value = state.window_state.capture_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_release_capture(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    state.window_state.capture_window_handle = 0;

    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ReleaseCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_update_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for UpdateWindow")?;

    let success = is_known_window(state, window_handle);

    if success {
        state.window_state.window_invalidated = false;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from UpdateWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_invalidate_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for InvalidateRect")?;

    let _rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for InvalidateRect")?;

    let _erase_background = engine
        .read_r8()
        .context("failed to read R8 for InvalidateRect")?;

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if success {
        state.window_state.window_invalidated = true;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from InvalidateRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_redraw_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for RedrawWindow")?;

    let _update_rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RedrawWindow")?;

    let _update_region = engine
        .read_r8()
        .context("failed to read R8 for RedrawWindow")?;

    let _flags = engine
        .read_r9()
        .context("failed to read R9 for RedrawWindow")?;

    let success = window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state.window_invalidated = false;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from RedrawWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_window_text_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTextA")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTextA")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && text_ptr != 0;

    if success {
        state.window_state.window_title = read_guest_ansi_lossy(engine, text_ptr, 32_768)
            .context("failed to read SetWindowTextA text")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowTextA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_window_text_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTextW")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTextW")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && text_ptr != 0;

    if success {
        state.window_state.window_title = read_guest_utf16_lossy(engine, text_ptr, 32_768)
            .context("failed to read SetWindowTextW text")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowTextW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_window_text_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextA")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowTextA")?;

    let max_characters = engine
        .read_r8()
        .context("failed to read R8 for GetWindowTextA")?;

    let return_value = if window_handle == FAKE_WINDOW_HANDLE {
        write_ansi_window_text(
            engine,
            buffer_ptr,
            max_characters,
            &state.window_state.window_title,
        )?
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowTextA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_window_text_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextW")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowTextW")?;

    let max_characters = engine
        .read_r8()
        .context("failed to read R8 for GetWindowTextW")?;

    let return_value = if window_handle == FAKE_WINDOW_HANDLE {
        write_wide_window_text(
            engine,
            buffer_ptr,
            max_characters,
            &state.window_state.window_title,
        )?
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowTextW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_client_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetClientRect")?;

    let rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetClientRect")?;

    let success = is_known_window(state, window_handle) && rect_ptr != 0;

    if success {
        let (width, height) = window_client_size(state, window_handle);
        write_window_rect(engine, rect_ptr, 0, 0, width, height)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetClientRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_move_window(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for MoveWindow")?;

    let x_raw = engine
        .read_rdx()
        .context("failed to read RDX for MoveWindow")?;

    let y_raw = engine
        .read_r8()
        .context("failed to read R8 for MoveWindow")?;

    let width_raw = engine
        .read_r9()
        .context("failed to read R9 for MoveWindow")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for MoveWindow")?;

    let height_arg_address = rsp
        .checked_add(0x28)
        .context("MoveWindow height argument address overflow")?;

    let repaint_arg_address = rsp
        .checked_add(0x30)
        .context("MoveWindow repaint argument address overflow")?;

    let height_raw = read_guest_u64(engine, height_arg_address)?;
    let repaint_raw = read_guest_u64(engine, repaint_arg_address)?;

    let success = window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state.window_x = low_i32(x_raw, "MoveWindow x")?;
        state.window_state.window_y = low_i32(y_raw, "MoveWindow y")?;
        state.window_state.window_width = low_i32(width_raw, "MoveWindow width")?;
        state.window_state.window_height = low_i32(height_raw, "MoveWindow height")?;

        if repaint_raw != 0 {
            state.window_state.window_invalidated = false;
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from MoveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_screen_to_client(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ScreenToClient")?;

    let point_ptr = engine
        .read_rdx()
        .context("failed to read RDX for ScreenToClient")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && point_ptr != 0;

    if success {
        let x = read_guest_i32(engine, point_ptr)?;

        let y_address = checked_field_address(point_ptr, 4, "POINT.y")?;

        let y = read_guest_i32(engine, y_address)?;

        let client_x = x
            .checked_sub(state.window_state.window_x)
            .context("ScreenToClient x coordinate overflow")?;

        let client_y = y
            .checked_sub(state.window_state.window_y)
            .context("ScreenToClient y coordinate overflow")?;

        write_guest_i32(engine, point_ptr, client_x)?;
        write_guest_i32(engine, y_address, client_y)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ScreenToClient")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_client_to_screen(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ClientToScreen")?;

    let point_ptr = engine
        .read_rdx()
        .context("failed to read RDX for ClientToScreen")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && point_ptr != 0;

    if success {
        let x = read_guest_i32(engine, point_ptr)?;

        let y_address = checked_field_address(point_ptr, 4, "POINT.y")?;

        let y = read_guest_i32(engine, y_address)?;

        let screen_x = x
            .checked_add(state.window_state.window_x)
            .context("ClientToScreen x coordinate overflow")?;

        let screen_y = y
            .checked_add(state.window_state.window_y)
            .context("ClientToScreen y coordinate overflow")?;

        write_guest_i32(engine, point_ptr, screen_x)?;
        write_guest_i32(engine, y_address, screen_y)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ClientToScreen")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_desktop_window(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(FAKE_DESKTOP_WINDOW_HANDLE)
        .context("failed to return from GetDesktopWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_DESKTOP_WINDOW_HANDLE,
    })
}
pub fn handle_get_sys_color(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let color_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSysColor")?;

    let return_value = match color_index {
        // Black-like colors:
        // COLOR_BACKGROUND, COLOR_WINDOWFRAME,
        // COLOR_MENUTEXT, COLOR_WINDOWTEXT,
        // COLOR_CAPTIONTEXT, COLOR_BTNTEXT.
        1 | 6 | 7..=9 | 18 => 0x0000_0000,

        // Accent colors:
        // COLOR_ACTIVECAPTION, COLOR_HIGHLIGHT.
        2 | 13 => 0x00d7_7830,

        // COLOR_INACTIVECAPTION.
        3 => 0x00bf_bfbf,

        // White-like colors:
        // COLOR_WINDOW, COLOR_HIGHLIGHTTEXT.
        5 | 14 => 0x00ff_ffff,

        // COLOR_ACTIVEBORDER, COLOR_INACTIVEBORDER.
        10 | 11 => 0x00b4_b4b4,

        // COLOR_APPWORKSPACE.
        12 => 0x00ab_abab,

        // COLOR_BTNSHADOW.
        16 => 0x00a0_a0a0,

        // COLOR_GRAYTEXT.
        17 => 0x006d_6d6d,

        // COLOR_SCROLLBAR.
        0 => 0x00c8_c8c8,

        // COLOR_MENU, COLOR_BTNFACE and neutral fallback.
        _ => 0x00f0_f0f0,
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSysColor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_sys_color_brush(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let color_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSysColorBrush")?;

    let return_value = FAKE_SYSTEM_COLOR_BRUSH_BASE
        .checked_add(color_index)
        .context("GetSysColorBrush handle overflow")?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSysColorBrush")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_rect(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetRect")?;

    let left_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetRect")?;

    let top_raw = engine.read_r8().context("failed to read R8 for SetRect")?;

    let right_raw = engine.read_r9().context("failed to read R9 for SetRect")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for SetRect")?;

    let bottom_address = rsp
        .checked_add(0x28)
        .context("SetRect bottom argument address overflow")?;

    let bottom_raw = read_guest_u64(engine, bottom_address)?;

    let success = rect_ptr != 0;

    if success {
        write_window_rect(
            engine,
            rect_ptr,
            low_i32(left_raw, "SetRect left")?,
            low_i32(top_raw, "SetRect top")?,
            low_i32(right_raw, "SetRect right")?,
            low_i32(bottom_raw, "SetRect bottom")?,
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_is_iconic(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsIconic")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsIconic")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_is_zoomed(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsZoomed")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsZoomed")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_window_thread_process_id(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowThreadProcessId")?;

    let process_id_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowThreadProcessId")?;

    let valid_window =
        window_handle == FAKE_WINDOW_HANDLE || window_handle == FAKE_DESKTOP_WINDOW_HANDLE;

    if valid_window && process_id_ptr != 0 {
        write_guest_u32(engine, process_id_ptr, FAKE_PROCESS_ID)?;
    }

    let return_value = if valid_window { FAKE_THREAD_ID } else { 0 };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowThreadProcessId")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_dlg_ctrl_id(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetDlgCtrlID")?;

    // The current single-window model has no child-control identifier.
    let return_value = if window_handle == FAKE_WINDOW_HANDLE {
        0
    } else {
        u64::from(u32::MAX)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDlgCtrlID")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_is_child(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _parent_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsChild")?;

    let _child_handle = engine
        .read_rdx()
        .context("failed to read RDX for IsChild")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsChild")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_window(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindow")?;

    let _command = engine
        .read_rdx()
        .context("failed to read RDX for GetWindow")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_window_long_ptr_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_window_long_ptr_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_set_window_long_ptr_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_adjust_window_rect_ex(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for AdjustWindowRectEx")?;

    let _style = engine
        .read_rdx()
        .context("failed to read RDX for AdjustWindowRectEx")?;

    let has_menu = engine
        .read_r8()
        .context("failed to read R8 for AdjustWindowRectEx")?;

    let _extended_style = engine
        .read_r9()
        .context("failed to read R9 for AdjustWindowRectEx")?;

    let success = rect_ptr != 0;

    if success {
        let left = read_guest_i32(engine, rect_ptr)?;

        let top_address = checked_field_address(rect_ptr, 4, "RECT.top")?;
        let right_address = checked_field_address(rect_ptr, 8, "RECT.right")?;
        let bottom_address = checked_field_address(rect_ptr, 12, "RECT.bottom")?;

        let top = read_guest_i32(engine, top_address)?;
        let right = read_guest_i32(engine, right_address)?;
        let bottom = read_guest_i32(engine, bottom_address)?;

        // Approximate classic non-client metrics:
        // 8 px frame on each side, 31 px caption,
        // and another 20 px when a menu is present.
        let menu_height = if has_menu != 0 { 20 } else { 0 };

        let adjusted_left = left
            .checked_sub(8)
            .context("AdjustWindowRectEx left overflow")?;

        let adjusted_top = top
            .checked_sub(31)
            .and_then(|value| value.checked_sub(menu_height))
            .context("AdjustWindowRectEx top overflow")?;

        let adjusted_right = right
            .checked_add(8)
            .context("AdjustWindowRectEx right overflow")?;

        let adjusted_bottom = bottom
            .checked_add(8)
            .context("AdjustWindowRectEx bottom overflow")?;

        write_window_rect(
            engine,
            rect_ptr,
            adjusted_left,
            adjusted_top,
            adjusted_right,
            adjusted_bottom,
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from AdjustWindowRectEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_scroll_window_ex(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for ScrollWindowEx")?;

    // Returns TRUE on success.
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ScrollWindowEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn find_window(state: &WinApiState, handle: u64) -> Option<&WindowRecord> {
    state
        .window_state
        .windows
        .iter()
        .find(|window| window.handle == handle)
}
