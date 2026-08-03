use super::{
    Context, DIALOG_BASE_UNIT_X, DIALOG_BASE_UNIT_Y, FAKE_CURSOR_HANDLE, FAKE_ICON_HANDLE,
    FAKE_IMAGE_HANDLE, HandlerContext, IDOK, Result, TimerRecord, WinApiHandlerResult, WinApiState,
    WindowClassRecord, WindowsHookRecord, checked_field_address, low_i32, read_guest_ansi_lossy,
    read_guest_i32, read_guest_u32, read_guest_u64, read_guest_utf16_lossy, register_window_class,
    write_guest_ansi_c_string, write_guest_utf16_c_string,
};

/// Handles `USER32.dll!LoadIconA`.
pub fn handle_load_icon_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_icon_impl(ctx, "LoadIconA", false)
}
/// Handles `USER32.dll!LoadIconW`.
pub fn handle_load_icon_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_icon_impl(ctx, "LoadIconW", true)
}

/// Shared `LoadIconA/W` implementation.
///
/// Win64 ABI: `rcx` = hinst, `rdx` = icon name. The name is either a
/// MAKEINTRESOURCE (high 16 bits zero → the low word is the resource id) or a
/// string pointer (UTF-8/CP1252 for A, UTF-16LE for W). Icons are not parsed
/// yet, so every request resolves to the shared fake icon handle; the decode
/// exists to consume the argument faithfully and log what real apps ask for.
fn handle_load_icon_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let icon_name_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let (icon_id, icon_name) = if icon_name_raw >> 16 == 0 {
        // MAKEINTRESOURCE: only the low word carries the resource id.
        let resource_id = u16::try_from(icon_name_raw & 0xFFFF).unwrap_or(0);
        (resource_id, String::new())
    } else if wide {
        let name = read_guest_utf16_lossy(engine, icon_name_raw, 64)?;
        (0, name)
    } else {
        let name = read_guest_ansi_lossy(engine, icon_name_raw, 64)?;
        (0, name)
    };

    tracing::debug!(
        target: "wiegui",
        instance_handle,
        icon_id,
        icon_name = %icon_name,
        "{api_name}"
    );

    let return_address = engine
        .return_from_win64_api(FAKE_ICON_HANDLE)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_ICON_HANDLE,
    })
}
/// Handles `USER32.dll!LoadCursorA`.
pub fn handle_load_cursor_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_cursor_impl(ctx, "LoadCursorA", false)
}
/// Handles `USER32.dll!LoadCursorW`.
pub fn handle_load_cursor_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_cursor_impl(ctx, "LoadCursorW", true)
}

/// Shared `LoadCursorA/W` implementation.
///
/// Win64 ABI: `rcx` = hinst, `rdx` = cursor name. The name is either a
/// MAKEINTRESOURCE (high 16 bits zero → the low word is the resource id) or a
/// string pointer (UTF-8/CP1252 for A, UTF-16LE for W). Cursors are not parsed
/// yet, so every request resolves to the shared fake cursor handle; the decode
/// exists to consume the argument faithfully and log what real apps ask for.
fn handle_load_cursor_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let cursor_name_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let (cursor_id, cursor_name) = if cursor_name_raw >> 16 == 0 {
        // MAKEINTRESOURCE: only the low word carries the resource id.
        let resource_id = u16::try_from(cursor_name_raw & 0xFFFF).unwrap_or(0);
        (resource_id, String::new())
    } else if wide {
        let name = read_guest_utf16_lossy(engine, cursor_name_raw, 64)?;
        (0, name)
    } else {
        let name = read_guest_ansi_lossy(engine, cursor_name_raw, 64)?;
        (0, name)
    };

    tracing::debug!(
        target: "wiegui",
        instance_handle,
        cursor_id,
        cursor_name = %cursor_name,
        "{api_name}"
    );

    let return_address = engine
        .return_from_win64_api(FAKE_CURSOR_HANDLE)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_CURSOR_HANDLE,
    })
}
/// Handles `USER32.dll!RegisterClassExW`.
pub fn handle_register_class_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
            checked_field_address(window_class_ptr, 4, "WNDCLASSEXW.style"),
        )
        .context("failed to read WNDCLASSEXW.style")?;

        let window_proc = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 8, "WNDCLASSEXW.lpfnWndProc"),
        )
        .context("failed to read WNDCLASSEXW.lpfnWndProc")?;

        let instance_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 24, "WNDCLASSEXW.hInstance"),
        )
        .context("failed to read WNDCLASSEXW.hInstance")?;

        let icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 32, "WNDCLASSEXW.hIcon"),
        )
        .context("failed to read WNDCLASSEXW.hIcon")?;

        let cursor_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 40, "WNDCLASSEXW.hCursor"),
        )
        .context("failed to read WNDCLASSEXW.hCursor")?;

        let background_brush = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 48, "WNDCLASSEXW.hbrBackground"),
        )
        .context("failed to read WNDCLASSEXW.hbrBackground")?;

        let menu_name = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 56, "WNDCLASSEXW.lpszMenuName"),
        )
        .context("failed to read WNDCLASSEXW.lpszMenuName")?;

        let class_name_ptr = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 64, "WNDCLASSEXW.lpszClassName"),
        )
        .context("failed to read WNDCLASSEXW.lpszClassName")?;

        let small_icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 72, "WNDCLASSEXW.hIconSm"),
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
                menu_name,
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
/// Handles `USER32.dll!RegisterClassExA`.
pub fn handle_register_class_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassExA")?;

    let return_value = if window_class_ptr == 0 {
        0
    } else {
        /*
         * WNDCLASSEXA on Win64 (same layout as WNDCLASSEXW, but lpszMenuName
         * and lpszClassName point to ANSI strings):
         * +0x00 UINT      cbSize
         * +0x04 UINT      style
         * +0x08 WNDPROC   lpfnWndProc
         * +0x10 INT       cbClsExtra
         * +0x14 INT       cbWndExtra
         * +0x18 HINSTANCE hInstance
         * +0x20 HICON     hIcon
         * +0x28 HCURSOR   hCursor
         * +0x30 HBRUSH    hbrBackground
         * +0x38 LPCSTR    lpszMenuName
         * +0x40 LPCSTR    lpszClassName
         * +0x48 HICON     hIconSm
         */
        let style = read_guest_u32(
            engine,
            checked_field_address(window_class_ptr, 4, "WNDCLASSEXA.style"),
        )
        .context("failed to read WNDCLASSEXA.style")?;

        let window_proc = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 8, "WNDCLASSEXA.lpfnWndProc"),
        )
        .context("failed to read WNDCLASSEXA.lpfnWndProc")?;

        let instance_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 24, "WNDCLASSEXA.hInstance"),
        )
        .context("failed to read WNDCLASSEXA.hInstance")?;

        let icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 32, "WNDCLASSEXA.hIcon"),
        )
        .context("failed to read WNDCLASSEXA.hIcon")?;

        let cursor_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 40, "WNDCLASSEXA.hCursor"),
        )
        .context("failed to read WNDCLASSEXA.hCursor")?;

        let background_brush = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 48, "WNDCLASSEXA.hbrBackground"),
        )
        .context("failed to read WNDCLASSEXA.hbrBackground")?;

        let menu_name = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 56, "WNDCLASSEXA.lpszMenuName"),
        )
        .context("failed to read WNDCLASSEXA.lpszMenuName")?;

        let class_name_ptr = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 64, "WNDCLASSEXA.lpszClassName"),
        )
        .context("failed to read WNDCLASSEXA.lpszClassName")?;

        let small_icon_handle = read_guest_u64(
            engine,
            checked_field_address(window_class_ptr, 72, "WNDCLASSEXA.hIconSm"),
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
                menu_name,
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
/// Handles `USER32.dll!MessageBoxW`.
///
/// A host-registered bridge (`GuestHandle::set_message_box_bridge`, the GUI
/// presenter's rfd native alert) shows the message and returns the Win32 id
/// the user chose; the guest thread blocks until then, which is correct
/// MessageBox semantics. Without a bridge (headless runs, `trace`) the message
/// echoes to the host console and the handler returns IDOK so no guest ever
/// hangs on a missing host.
pub fn handle_message_box_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (caption, text, message_box_type) = {
        let engine = &mut *ctx.engine;
        let _window_handle = engine
            .read_rcx()
            .context("failed to read RCX for MessageBoxW")?;

        let text_ptr = engine
            .read_rdx()
            .context("failed to read RDX for MessageBoxW")?;

        let caption_ptr = engine
            .read_r8()
            .context("failed to read R8 for MessageBoxW")?;

        let message_box_type_raw = engine
            .read_r9()
            .context("failed to read R9 for MessageBoxW")?;

        let text = read_guest_utf16_lossy(engine, text_ptr, 1024)
            .context("failed to read MessageBoxW text")?;

        let caption = read_guest_utf16_lossy(engine, caption_ptr, 256)
            .context("failed to read MessageBoxW caption")?;

        (
            caption,
            text,
            u32::try_from(message_box_type_raw).unwrap_or(0),
        )
    };

    tracing::info!(caption = %caption, text = %text, message_box_type, "MessageBoxW");
    let win32_id = message_box_result(ctx, &caption, &text, message_box_type, "MessageBoxW");

    let return_address = ctx
        .engine
        .return_from_win64_api(win32_id)
        .context("failed to return from MessageBoxW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: win32_id,
    })
}
/// Handles `USER32.dll!MessageBoxA`.
pub fn handle_message_box_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (caption, text, message_box_type) = {
        let engine = &mut *ctx.engine;
        let _window_handle = engine
            .read_rcx()
            .context("failed to read RCX for MessageBoxA")?;

        let text_ptr = engine
            .read_rdx()
            .context("failed to read RDX for MessageBoxA")?;

        let caption_ptr = engine
            .read_r8()
            .context("failed to read R8 for MessageBoxA")?;

        let message_box_type_raw = engine
            .read_r9()
            .context("failed to read R9 for MessageBoxA")?;

        let text = read_guest_ansi_lossy(engine, text_ptr, 1024)
            .context("failed to read MessageBoxA text")?;

        let caption = read_guest_ansi_lossy(engine, caption_ptr, 256)
            .context("failed to read MessageBoxA caption")?;

        (
            caption,
            text,
            u32::try_from(message_box_type_raw).unwrap_or(0),
        )
    };

    tracing::info!(caption = %caption, text = %text, message_box_type, "MessageBoxA");
    let win32_id = message_box_result(ctx, &caption, &text, message_box_type, "MessageBoxA");

    let return_address = ctx
        .engine
        .return_from_win64_api(win32_id)
        .context("failed to return from MessageBoxA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: win32_id,
    })
}

/// Route a decoded MessageBox to the host bridge, or the console-echo fallback.
///
/// The `mb_type` argument passes through verbatim — the host bridge (wie-cli,
/// where rfd lives) maps MB_* flag bits to its button/level sets, keeping rfd
/// types out of this crate. The bridge's returned Win32 id (already the id the
/// guest expects) is handed back; without a bridge the message echoes to the
/// host console (7z bring-up behavior) and the handler auto-answers IDOK.
fn message_box_result(
    ctx: &mut HandlerContext<'_>,
    caption: &str,
    text: &str,
    message_box_type: u32,
    api_name: &str,
) -> u64 {
    if let Some(bridge) = ctx
        .state
        .try_present()
        .and_then(|present| present.message_box_bridge.as_ref())
    {
        let win32_id = bridge(caption, text, message_box_type);
        return u64::try_from(i64::from(win32_id)).unwrap_or(IDOK);
    }
    // Always surface guest error UI on host console (7z bring-up); no bridge
    // means headless/trace — auto-OK so the guest never hangs.
    eprintln!("[{api_name}] {caption}: {text}");
    IDOK
}
/// Handles dynamic `USER32.dll!SetProcessDPIAware`.
pub fn handle_set_process_dpi_aware(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from SetProcessDPIAware")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!LoadImageA`.
pub fn handle_load_image_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_image(ctx, "LoadImageA")
}
/// Handles `USER32.dll!LoadImageW`.
pub fn handle_load_image_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_image(ctx, "LoadImageW")
}
pub(crate) fn handle_load_image(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
/// Handles `USER32.dll!DestroyIcon`.
pub fn handle_destroy_icon(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
/// Handles `USER32.dll!GetDialogBaseUnits`.
pub fn handle_get_dialog_base_units(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_value = u64::from(DIALOG_BASE_UNIT_X | (DIALOG_BASE_UNIT_Y << 16));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDialogBaseUnits")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetTimer`.
pub fn handle_set_timer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetTimer")?;

    let requested_timer_id = engine
        .read_rdx()
        .context("failed to read RDX for SetTimer")?;

    let interval_raw = engine.read_r8().context("failed to read R8 for SetTimer")?;

    let callback_address = engine.read_r9().context("failed to read R9 for SetTimer")?;

    // Thread timers (hwnd == 0) and any known window are accepted.
    let valid_window = window_handle == 0 || super::is_known_window(state, window_handle);

    let interval_low = interval_raw & u64::from(u32::MAX);

    let interval_ms = u32::try_from(interval_low).context("SetTimer interval does not fit u32")?;

    let return_value = if valid_window {
        let timer_id = if requested_timer_id == 0 {
            let generated_id = state.window_state().next_timer_id;

            state.window_state().next_timer_id = state
                .window_state()
                .next_timer_id
                .checked_add(1)
                .context("SetTimer identifier overflow")?;

            generated_id
        } else {
            requested_timer_id
        };

        if let Some(timer) = state.window_state().timers.iter_mut().find(|timer| {
            timer.window_handle == crate::handles::Hwnd::from(window_handle)
                && timer.timer_id == timer_id
        }) {
            timer.interval_ms = interval_ms;
            timer.callback_address = callback_address;
            timer.next_fire = timer_deadline(interval_ms);
        } else {
            state.window_state().timers.push(TimerRecord {
                window_handle: crate::handles::Hwnd::from(window_handle),
                timer_id,
                interval_ms,
                callback_address,
                next_fire: timer_deadline(interval_ms),
            });
        }

        tracing::debug!(
            target: "wiegui",
            hwnd = window_handle,
            timer_id,
            interval_ms,
            "SetTimer"
        );

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
/// Handles `USER32.dll!KillTimer`.
pub fn handle_kill_timer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for KillTimer")?;

    let timer_id = engine
        .read_rdx()
        .context("failed to read RDX for KillTimer")?;

    let existed = state.window_state().timers.iter().any(|timer| {
        timer.window_handle == crate::handles::Hwnd::from(window_handle)
            && timer.timer_id == timer_id
    });

    tracing::debug!(
        target: "wiegui",
        hwnd = window_handle,
        timer_id,
        existed,
        "KillTimer"
    );

    if existed {
        state.window_state().timers.retain(|timer| {
            timer.window_handle != crate::handles::Hwnd::from(window_handle)
                || timer.timer_id != timer_id
        });
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
/// Handles `USER32.dll!SetWindowsHookExW`.
pub fn handle_set_windows_hook_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
        let handle = state.window_state().next_windows_hook_handle.as_u64();

        state.window_state().next_windows_hook_handle = crate::handles::HookHandle::from(
            state
                .window_state()
                .next_windows_hook_handle
                .as_u64()
                .checked_add(1)
                .context("SetWindowsHookExW handle overflow")?,
        );

        state.window_state().windows_hooks.push(WindowsHookRecord {
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
/// Handles `USER32.dll!UnhookWindowsHookEx`.
pub fn handle_unhook_windows_hook_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hook_handle = engine
        .read_rcx()
        .context("failed to read RCX for UnhookWindowsHookEx")?;

    let existed = state
        .window_state()
        .windows_hooks
        .iter()
        .any(|hook| hook.handle == hook_handle);

    if existed {
        state
            .window_state()
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
/// Handles `USER32.dll!SetScrollInfo`.
pub fn handle_set_scroll_info(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
            checked_field_address(scroll_info_ptr, 20, "SCROLLINFO.nPos"),
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
/// One past the last id `RegisterWindowMessageA/W` may return (0xFFFF).
///
/// The first id handed out is 0xC000 (the start of the Windows-reserved
/// range); that seed lives in `WindowState::default` as
/// `next_registered_message`.
const REGISTERED_MESSAGE_LIMIT: u32 = 0x1_0000;

/// Handles `USER32.dll!RegisterWindowMessageW`.
pub fn handle_register_window_message_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_register_window_message(ctx, "RegisterWindowMessageW")
}
/// Handles `USER32.dll!RegisterWindowMessageA`.
pub fn handle_register_window_message_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_register_window_message(ctx, "RegisterWindowMessageA")
}

fn handle_register_window_message(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let name = if api_name.ends_with('W') {
        read_guest_utf16_lossy(engine, name_ptr, 256)
            .with_context(|| format!("failed to read {api_name} message name"))?
    } else {
        read_guest_ansi_lossy(engine, name_ptr, 256)
            .with_context(|| format!("failed to read {api_name} message name"))?
    };

    // Windows matches registered-message names case-insensitively, so the
    // cache key is the lowercased name — the A and W variants share one entry.
    let key = name.to_ascii_lowercase();

    let return_value = if let Some(id) = state.window_state().registered_messages.get(&key) {
        u64::from(*id)
    } else if state.window_state().next_registered_message >= REGISTERED_MESSAGE_LIMIT {
        // The reserved range is exhausted; Windows returns zero on failure.
        0
    } else {
        let id = state.window_state().next_registered_message;

        state.window_state().next_registered_message = state
            .window_state()
            .next_registered_message
            .checked_add(1)
            .context("registered-message id overflow")?;

        state.window_state().registered_messages.insert(key, id);

        u64::from(id)
    };

    tracing::debug!(
        target: "wiegui",
        name = %name,
        message_id = return_value,
        "{api_name}"
    );

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!LoadStringW`.
pub fn handle_load_string_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_string(ctx, "LoadStringW")
}
/// Handles `USER32.dll!LoadStringA`.
pub fn handle_load_string_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_string(ctx, "LoadStringA")
}

/// Shared `LoadStringA/W` implementation.
///
/// Win64 ABI: `rcx` = hinst, `rdx` = string id, `r8` = output buffer,
/// `r9` = `cchMax`. Copies the module string into the guest buffer (UTF-16 as
/// stored in the resource for W; the ANSI/UTF-8 convention for A), truncates
/// to `cchMax - 1` chars, NUL-terminates, and returns the number of characters
/// copied excluding the NUL — 0 when the id is not found or the module is not
/// the main EXE (loaded-DLL string tables are not parsed yet).
fn handle_load_string(ctx: &mut HandlerContext<'_>, api_name: &str) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let string_id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let buffer_ptr = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let max_characters_raw = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    // API contract: LoadString string ids are u16 (anything wider cannot
    // address a parsed block).
    let string_id = u16::try_from(string_id_raw & u64::from(u16::MAX)).unwrap_or(0);
    let max_characters = usize::try_from(max_characters_raw)
        .with_context(|| format!("{api_name} cchMax does not fit usize"))?;

    let text = resolve_string_text(
        state,
        ctx.environment.image_base,
        instance_handle,
        string_id,
    );

    let return_value = if text.is_empty() {
        // Not found (or empty string): Windows returns 0 either way.
        0
    } else if api_name.ends_with('W') {
        let copied = write_guest_utf16_c_string(engine, buffer_ptr, max_characters, &text)?;
        u64::try_from(copied).unwrap_or(0)
    } else {
        // A-strings follow the codebase UTF-8 convention; the byte count
        // equals the character count for the ASCII strings real apps load.
        let copied = write_guest_ansi_c_string(engine, buffer_ptr, max_characters, &text)?;
        u64::try_from(copied).unwrap_or(0)
    };

    tracing::debug!(
        target: "wiegui",
        instance_handle,
        string_id,
        text = %text,
        copied = return_value,
        "{api_name}"
    );

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Look up a string-table entry by id for `hinst`.
///
/// Only the main EXE module's table is parsed (mirrors the dialog/menu
/// lifecycle); any other `hinst` resolves to not-found. Block names are
/// 1-based in real rc.exe output, so the block id is `(id >> 4) + 1`. When a
/// block exists in several locales the UI language picks the block (exact
/// LANGID → neutral → en-US → first in directory order).
fn resolve_string_text(
    state: &WinApiState,
    image_base: u64,
    instance_handle: u64,
    string_id: u16,
) -> String {
    if instance_handle != image_base {
        return String::new();
    }
    let block_id = (string_id >> 4).saturating_add(1);
    let slot = usize::from(string_id & 0xF);
    let ui_language = super::lang::ui_language();
    let block = super::lang::resolve_block(
        state
            .process
            .main_module_strings
            .iter()
            .filter(|block| block.block == block_id)
            .map(|block| (u32::from(block.lang), block)),
        ui_language,
    );
    block
        .and_then(|block| block.strings.get(slot))
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn window_client_size(state: &mut WinApiState, handle: u64) -> (i32, i32) {
    // Extract window dimensions before any other mutable access.
    if let Some(window) = super::find_window(state, handle) {
        let width = if window.width > 0 {
            window.width
        } else {
            return (1, 1);
        };
        let height = if window.height > 0 {
            window.height
        } else {
            return (1, 1);
        };
        return (width.max(1), height.max(1));
    }
    (
        state.window_state().window_width.max(1),
        state.window_state().window_height.max(1),
    )
}

/// Host-clock deadline for the next timer fire, `interval_ms` from now.
///
/// A zero interval is floored to 1 ms so a pathological `SetTimer(…, 0, …)`
/// cannot busy-loop the message pump.
#[must_use]
pub(crate) fn timer_deadline(interval_ms: u32) -> std::time::Instant {
    let delay = std::time::Duration::from_millis(u64::from(interval_ms.max(1)));
    let now = std::time::Instant::now();
    now.checked_add(delay).unwrap_or_else(|| {
        now.checked_add(std::time::Duration::from_secs(1))
            .unwrap_or(now)
    })
}
