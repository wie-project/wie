use super::{
    Context, DIALOG_BASE_UNIT_X, DIALOG_BASE_UNIT_Y, FAKE_CURSOR_HANDLE, FAKE_ICON_HANDLE,
    FAKE_IMAGE_HANDLE, FAKE_WINDOW_HANDLE, IDOK, Result, TimerRecord, WinApiHandlerResult,
    WinApiState, WindowClassRecord, WindowsHookRecord, checked_field_address, low_i32,
    read_guest_ansi_lossy, read_guest_i32, read_guest_u32, read_guest_u64, read_guest_utf16_lossy,
    register_window_class,
};

pub fn handle_load_icon_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _instance_handle = engine
        .read_rcx()
        .context("failed to read RCX for LoadIconA")?;

    let _icon_name = engine
        .read_rdx()
        .context("failed to read RDX for LoadIconA")?;

    let return_address = engine
        .return_from_win64_api(FAKE_ICON_HANDLE)
        .context("failed to return from LoadIconA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_ICON_HANDLE,
    })
}
pub fn handle_load_cursor_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _instance_handle = engine
        .read_rcx()
        .context("failed to read RCX for LoadCursorA")?;

    let _cursor_name = engine
        .read_rdx()
        .context("failed to read RDX for LoadCursorA")?;

    let return_address = engine
        .return_from_win64_api(FAKE_CURSOR_HANDLE)
        .context("failed to return from LoadCursorA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_CURSOR_HANDLE,
    })
}
pub fn handle_register_class_ex_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassExW")?;

    let return_value = if window_class_ptr == 0 {
        0
    } else {
        /*
         * WNDCLASSEXW on Win64:
         * +0x00 UINT      cbSize
         * +0x04 UINT      style
         * +0x08 WNDPROC   lpfnWndProc
         * +0x10 INT       cbClsExtra
         * +0x14 INT       cbWndExtra
         * +0x18 HINSTANCE hInstance
         * +0x20 HICON     hIcon
         * +0x28 HCURSOR   hCursor
         * +0x30 HBRUSH    hbrBackground
         * +0x38 LPCWSTR   lpszMenuName
         * +0x40 LPCWSTR   lpszClassName
         * +0x48 HICON     hIconSm
         */

        let style = read_guest_u32(
            engine,
            checked_field_address(window_class_ptr, 4, "WNDCLASSEXW.style")?,
        )
        .context("failed to read WNDCLASSEXW.style")?;

        let window_proc = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 8, "WNDCLASSEXW.lpfnWndProc")?,
        )
        .context("failed to read WNDCLASSEXW.lpfnWndProc")?;

        let instance_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 24, "WNDCLASSEXW.hInstance")?,
        )
        .context("failed to read WNDCLASSEXW.hInstance")?;

        let icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 32, "WNDCLASSEXW.hIcon")?,
        )
        .context("failed to read WNDCLASSEXW.hIcon")?;

        let cursor_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 40, "WNDCLASSEXW.hCursor")?,
        )
        .context("failed to read WNDCLASSEXW.hCursor")?;

        let background_brush = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 48, "WNDCLASSEXW.hbrBackground")?,
        )
        .context("failed to read WNDCLASSEXW.hbrBackground")?;

        let class_name_ptr = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 64, "WNDCLASSEXW.lpszClassName")?,
        )
        .context("failed to read WNDCLASSEXW.lpszClassName")?;

        let small_icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 72, "WNDCLASSEXW.hIconSm")?,
        )
        .context("failed to read WNDCLASSEXW.hIconSm")?;

        let class_name = read_guest_utf16_lossy(engine, class_name_ptr, 256)
            .context("failed to read RegisterClassExW class name")?;

        register_window_class(
            state,
            WindowClassRecord {
                atom: 0,
                class_name,
                window_proc,
                style,
                instance_handle,
                icon_handle,
                cursor_handle,
                background_brush,
                small_icon_handle,
                unicode: true,
            },
        )?
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from RegisterClassExW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_register_class_ex_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassExA")?;

    let return_value = if window_class_ptr == 0 {
        0
    } else {
        let style = read_guest_u32(
            engine,
            checked_field_address(window_class_ptr, 4, "WNDCLASSEXA.style")?,
        )
        .context("failed to read WNDCLASSEXA.style")?;

        let window_proc = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 8, "WNDCLASSEXA.lpfnWndProc")?,
        )
        .context("failed to read WNDCLASSEXA.lpfnWndProc")?;

        let instance_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 24, "WNDCLASSEXA.hInstance")?,
        )
        .context("failed to read WNDCLASSEXA.hInstance")?;

        let icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 32, "WNDCLASSEXA.hIcon")?,
        )
        .context("failed to read WNDCLASSEXA.hIcon")?;

        let cursor_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 40, "WNDCLASSEXA.hCursor")?,
        )
        .context("failed to read WNDCLASSEXA.hCursor")?;

        let background_brush = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 48, "WNDCLASSEXA.hbrBackground")?,
        )
        .context("failed to read WNDCLASSEXA.hbrBackground")?;

        let class_name_ptr = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 64, "WNDCLASSEXA.lpszClassName")?,
        )
        .context("failed to read WNDCLASSEXA.lpszClassName")?;

        let small_icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 72, "WNDCLASSEXA.hIconSm")?,
        )
        .context("failed to read WNDCLASSEXA.hIconSm")?;

        let class_name = read_guest_ansi_lossy(engine, class_name_ptr, 256)
            .context("failed to read RegisterClassExA class name")?;

        register_window_class(
            state,
            WindowClassRecord {
                atom: 0,
                class_name,
                window_proc,
                style,
                instance_handle,
                icon_handle,
                cursor_handle,
                background_brush,
                small_icon_handle,
                unicode: false,
            },
        )?
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from RegisterClassExA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_message_box_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for MessageBoxW")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for MessageBoxW")?;

    let caption_ptr = engine
        .read_r8()
        .context("failed to read R8 for MessageBoxW")?;

    let _message_box_type = engine
        .read_r9()
        .context("failed to read R9 for MessageBoxW")?;

    let text = read_guest_utf16_lossy(engine, text_ptr, 1024)
        .context("failed to read MessageBoxW text")?;

    let caption = read_guest_utf16_lossy(engine, caption_ptr, 256)
        .context("failed to read MessageBoxW caption")?;

    tracing::info!(caption = %caption, text = %text, "MessageBoxW");
    // Always surface guest error UI on host console (7z bring-up).
    eprintln!("[MessageBoxW] {caption}: {text}");

    let return_address = engine
        .return_from_win64_api(IDOK)
        .context("failed to return from MessageBoxW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: IDOK,
    })
}
pub fn handle_message_box_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for MessageBoxA")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for MessageBoxA")?;

    let caption_ptr = engine
        .read_r8()
        .context("failed to read R8 for MessageBoxA")?;

    let _message_box_type = engine
        .read_r9()
        .context("failed to read R9 for MessageBoxA")?;

    let text =
        read_guest_ansi_lossy(engine, text_ptr, 1024).context("failed to read MessageBoxA text")?;

    let caption = read_guest_ansi_lossy(engine, caption_ptr, 256)
        .context("failed to read MessageBoxA caption")?;

    tracing::info!(caption = %caption, text = %text, "MessageBoxA");

    let return_address = engine
        .return_from_win64_api(IDOK)
        .context("failed to return from MessageBoxA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: IDOK,
    })
}
pub fn handle_set_process_dpi_aware(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from SetProcessDPIAware")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub fn handle_load_image_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_load_image(engine, "LoadImageA")
}
pub fn handle_load_image_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_load_image(engine, "LoadImageW")
}
pub(crate) fn handle_load_image(
    engine: &mut dyn wie_cpu::CpuEngine,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let _instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let _image_name_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let _image_type = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let _desired_width = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    // Win64 arguments 5 and 6 are desired height and load flags.
    // For bootstrap purposes, return a stable non-null image handle.
    let return_address = engine
        .return_from_win64_api(FAKE_IMAGE_HANDLE)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_IMAGE_HANDLE,
    })
}
pub fn handle_destroy_icon(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let icon_handle = engine
        .read_rcx()
        .context("failed to read RCX for DestroyIcon")?;

    let return_value = u64::from(icon_handle != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from DestroyIcon")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_dialog_base_units(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let return_value = u64::from(DIALOG_BASE_UNIT_X | (DIALOG_BASE_UNIT_Y << 16));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDialogBaseUnits")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_timer(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetTimer")?;

    let requested_timer_id = engine
        .read_rdx()
        .context("failed to read RDX for SetTimer")?;

    let interval_raw = engine.read_r8().context("failed to read R8 for SetTimer")?;

    let callback_address = engine.read_r9().context("failed to read R9 for SetTimer")?;

    let valid_window = window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE;

    let interval_low = interval_raw & u64::from(u32::MAX);

    let interval_ms = u32::try_from(interval_low).context("SetTimer interval does not fit u32")?;

    let return_value = if valid_window {
        let timer_id = if requested_timer_id == 0 {
            let generated_id = state.window_state.next_timer_id;

            state.window_state.next_timer_id = state
                .window_state
                .next_timer_id
                .checked_add(1)
                .context("SetTimer identifier overflow")?;

            generated_id
        } else {
            requested_timer_id
        };

        if let Some(timer) = state
            .window_state
            .timers
            .iter_mut()
            .find(|timer| timer.window_handle == window_handle && timer.timer_id == timer_id)
        {
            timer.interval_ms = interval_ms;
            timer.callback_address = callback_address;
        } else {
            state.window_state.timers.push(TimerRecord {
                window_handle,
                timer_id,
                interval_ms,
                callback_address,
            });
        }

        timer_id
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetTimer")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_kill_timer(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for KillTimer")?;

    let timer_id = engine
        .read_rdx()
        .context("failed to read RDX for KillTimer")?;

    let existed = state
        .window_state
        .timers
        .iter()
        .any(|timer| timer.window_handle == window_handle && timer.timer_id == timer_id);

    if existed {
        state
            .window_state
            .timers
            .retain(|timer| timer.window_handle != window_handle || timer.timer_id != timer_id);
    }

    let return_value = u64::from(existed);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from KillTimer")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_windows_hook_ex_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let hook_type_raw = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowsHookExW")?;

    let callback_address = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowsHookExW")?;

    let module_handle = engine
        .read_r8()
        .context("failed to read R8 for SetWindowsHookExW")?;

    let thread_id_raw = engine
        .read_r9()
        .context("failed to read R9 for SetWindowsHookExW")?;

    let hook_type = low_i32(hook_type_raw, "SetWindowsHookExW hook type")?;

    let thread_id_low = thread_id_raw & u64::from(u32::MAX);
    let thread_id = u32::try_from(thread_id_low)
        .context("SetWindowsHookExW thread identifier does not fit u32")?;

    let return_value = if callback_address == 0 {
        0
    } else {
        let handle = state.window_state.next_windows_hook_handle;

        state.window_state.next_windows_hook_handle = state
            .window_state
            .next_windows_hook_handle
            .checked_add(1)
            .context("SetWindowsHookExW handle overflow")?;

        state.window_state.windows_hooks.push(WindowsHookRecord {
            handle,
            hook_type,
            callback_address,
            module_handle,
            thread_id,
        });

        handle
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowsHookExW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_unhook_windows_hook_ex(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let hook_handle = engine
        .read_rcx()
        .context("failed to read RCX for UnhookWindowsHookEx")?;

    let existed = state
        .window_state
        .windows_hooks
        .iter()
        .any(|hook| hook.handle == hook_handle);

    if existed {
        state
            .window_state
            .windows_hooks
            .retain(|hook| hook.handle != hook_handle);
    }

    let return_value = u64::from(existed);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from UnhookWindowsHookEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_scroll_info(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetScrollInfo")?;

    let bar = engine
        .read_rdx()
        .context("failed to read RDX for SetScrollInfo")?;

    let scroll_info_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetScrollInfo")?;

    let _redraw = engine
        .read_r9()
        .context("failed to read R9 for SetScrollInfo")?;

    // SCROLLINFO (Win64):
    // UINT cbSize;    0
    // UINT fMask;     4
    // int  nMin;      8
    // int  nMax;      12
    // UINT nPage;     16
    // int  nPos;      20
    // int  nTrackPos; 24
    let return_value = if scroll_info_ptr != 0 {
        let n_pos = read_guest_i32(
            engine,
            checked_field_address(scroll_info_ptr, 20, "SCROLLINFO.nPos")?,
        )
        .unwrap_or(0);
        // Win32 returns the current scroll-box position after the update.
        // Bitcast i32 → u32 (two's complement), then zero-extend to RAX.
        u64::from(u32::from_le_bytes(n_pos.to_le_bytes()))
    } else {
        0
    };

    tracing::debug!(
        window_handle,
        bar,
        scroll_info_ptr,
        return_value,
        "SetScrollInfo"
    );

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetScrollInfo")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub(crate) fn window_client_size(state: &WinApiState, handle: u64) -> (i32, i32) {
    if let Some(window) = super::find_window(state, handle) {
        let width = if window.width > 0 {
            window.width
        } else {
            state.window_state.window_width
        };
        let height = if window.height > 0 {
            window.height
        } else {
            state.window_state.window_height
        };
        return (width.max(1), height.max(1));
    }
    (
        state.window_state.window_width.max(1),
        state.window_state.window_height.max(1),
    )
}
