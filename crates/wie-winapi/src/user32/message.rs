use super::{
    Context, FAKE_WINDOW_HANDLE, GuestCallbackRequest, HandlerContext, MessageQueueIdlePolicy,
    QueuedWindowMessage, Result, WM_CHAR, WM_DEADCHAR, WM_KEYDOWN, WM_KEYUP, WM_MDICREATE, WM_QUIT,
    WM_SYSCHAR, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WinApiControlSignal,
    WinApiHandlerResult, checked_field_address, create_mdi_child_from_struct, read_guest_u32,
    read_guest_u64, write_message_structure,
};

/// Handles `USER32.dll!PeekMessageA`.
pub fn handle_peek_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let message_address = engine
        .read_rcx()
        .context("failed to read RCX for PeekMessageA")?;

    let window_filter = engine
        .read_rdx()
        .context("failed to read RDX for PeekMessageA")?;

    let minimum_message_raw = engine
        .read_r8()
        .context("failed to read R8 for PeekMessageA")?;

    let maximum_message_raw = engine
        .read_r9()
        .context("failed to read R9 for PeekMessageA")?;

    let w_remove_msg = engine
        .read_rsp()
        .ok()
        .and_then(|rsp| read_guest_u32(engine, rsp.wrapping_add(0x28)).ok())
        .unwrap_or(0);

    let minimum_message = u32::try_from(minimum_message_raw & u64::from(u32::MAX))
        .context("PeekMessageA minimum message does not fit u32")?;

    let maximum_message = u32::try_from(maximum_message_raw & u64::from(u32::MAX))
        .context("PeekMessageA maximum message does not fit u32")?;

    let matches_filter = |queued: &QueuedWindowMessage| -> bool {
        let window_matches = window_filter == 0 || queued.window_handle == window_filter;
        let message_matches = if minimum_message == 0 && maximum_message == 0 {
            true
        } else {
            queued.message >= minimum_message && queued.message <= maximum_message
        };
        window_matches && message_matches
    };

    let matching_index = state
        .window_state
        .message_queue
        .iter()
        .position(matches_filter);

    let return_value = if let Some(index) = matching_index {
        let queued = if w_remove_msg != 0 {
            // PM_REMOVE: remove from queue.
            state.window_state.message_queue.remove(index)
        } else {
            // PM_NOREMOVE: leave in queue. Index is from `position` on this Vec.
            state
                .window_state
                .message_queue
                .get(index)
                .cloned()
                .context("PeekMessageA matching index vanished")?
        };
        write_message_structure(engine, message_address, &queued)?;
        1
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from PeekMessageA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!CallMsgFilterA/W`.
///
/// Returns FALSE so the message continues through the normal dispatch path
/// (no installed WH_MSGFILTER/WH_SYSMSGFILTER hooks).
pub fn handle_call_msg_filter(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _msg_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let _code = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let return_address = engine
        .return_from_win64_api(0)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `USER32.dll!PostMessageA`.
pub fn handle_post_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for PostMessageA")?;

    let message_raw = engine
        .read_rdx()
        .context("failed to read RDX for PostMessageA")?;

    let word_parameter = engine
        .read_r8()
        .context("failed to read R8 for PostMessageA")?;

    let long_parameter = engine
        .read_r9()
        .context("failed to read R9 for PostMessageA")?;

    let message = u32::try_from(message_raw & u64::from(u32::MAX))
        .context("PostMessageA message does not fit u32")?;

    let valid_window = window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE;

    if valid_window {
        let time = state.window_state.next_message_time;

        state.window_state.next_message_time = state
            .window_state
            .next_message_time
            .checked_add(1)
            .context("PostMessageA timestamp overflow")?;

        state.window_state.message_queue.push(QueuedWindowMessage {
            window_handle,
            message,
            word_parameter,
            long_parameter,
            time,
            point_x: 0,
            point_y: 0,
        });
    }

    let return_value = u64::from(valid_window);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from PostMessageA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SendMessageA`.
pub fn handle_send_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_send_message(ctx, false, "SendMessageA")
}
/// Handles `USER32.dll!SendMessageW`.
pub fn handle_send_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_send_message(ctx, true, "SendMessageW")
}
pub(crate) fn handle_send_message(
    ctx: &mut HandlerContext<'_>,
    prefer_unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let message_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let word_parameter = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let long_parameter = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    let message = u32::try_from(message_raw & u64::from(u32::MAX))
        .with_context(|| format!("{api_name} message does not fit u32"))?;

    // MDI client windows have no guest WndProc; WM_MDICREATE is handled here.
    if message == WM_MDICREATE {
        let child = create_mdi_child_from_struct(engine, state, long_parameter, prefer_unicode)
            .with_context(|| format!("failed to handle WM_MDICREATE in {api_name}"))?;

        let return_address = engine
            .return_from_win64_api(child)
            .with_context(|| format!("failed to return from {api_name} WM_MDICREATE"))?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: child,
        });
    }

    if let Some(target_window) = super::find_window(state, window_handle)
        && target_window.window_proc != 0
    {
        // Synchronous send: do not return yet; runtime bridges into WndProc.
        let unicode = if prefer_unicode {
            true
        } else {
            target_window.unicode
        };

        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: target_window.window_proc,
                window_handle,
                message,
                word_parameter,
                long_parameter,
                unicode,
            },
        }
        .into());
    }

    let return_address = engine
        .return_from_win64_api(0)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `USER32.dll!CallNextHookEx`.
pub fn handle_call_next_hook_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hook_handle = engine
        .read_rcx()
        .context("failed to read RCX for CallNextHookEx")?;

    let _code = engine
        .read_rdx()
        .context("failed to read RDX for CallNextHookEx")?;

    let _word_parameter = engine
        .read_r8()
        .context("failed to read R8 for CallNextHookEx")?;

    let _long_parameter = engine
        .read_r9()
        .context("failed to read R9 for CallNextHookEx")?;

    // There is currently no host-side hook chain after the guest hook.
    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CallNextHookEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetMessageA`.
pub fn handle_get_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let message_address = engine
        .read_rcx()
        .context("failed to read RCX for GetMessageA")?;

    let window_filter = engine
        .read_rdx()
        .context("failed to read RDX for GetMessageA")?;

    let minimum_message_raw = engine
        .read_r8()
        .context("failed to read R8 for GetMessageA")?;

    let maximum_message_raw = engine
        .read_r9()
        .context("failed to read R9 for GetMessageA")?;

    let minimum_message = u32::try_from(minimum_message_raw & u64::from(u32::MAX))
        .context("GetMessageA minimum message does not fit u32")?;

    let maximum_message = u32::try_from(maximum_message_raw & u64::from(u32::MAX))
        .context("GetMessageA maximum message does not fit u32")?;

    let matches_filter = |queued: &QueuedWindowMessage| -> bool {
        let window_matches = window_filter == 0 || queued.window_handle == window_filter;

        let message_matches = if minimum_message == 0 && maximum_message == 0 {
            true
        } else {
            queued.message >= minimum_message && queued.message <= maximum_message
        };

        window_matches && message_matches
    };

    let matching_index = state
        .window_state
        .message_queue
        .iter()
        .position(matches_filter);

    let return_value = if message_address == 0 {
        // GetMessage returns -1 on failure.
        u64::from(u32::MAX)
    } else if let Some(index) = matching_index {
        let queued = state.window_state.message_queue.remove(index);

        write_message_structure(engine, message_address, &queued)?;

        u64::from(queued.message != WM_QUIT)
    } else {
        match state.window_state.message_queue_idle_policy {
            MessageQueueIdlePolicy::ExitOnIdle => {
                /*
                 * Regression mode: represent an empty queue as a synthetic
                 * WM_QUIT so the guest performs its normal teardown.
                 */
                let quit_message = QueuedWindowMessage {
                    window_handle: 0,
                    message: WM_QUIT,
                    word_parameter: 0,
                    long_parameter: 0,
                    time: state.window_state.next_message_time,
                    point_x: 0,
                    point_y: 0,
                };

                state.window_state.next_message_time = state
                    .window_state
                    .next_message_time
                    .checked_add(1)
                    .context("GetMessageA timestamp overflow")?;

                write_message_structure(engine, message_address, &quit_message)?;

                0
            }

            MessageQueueIdlePolicy::YieldOnIdle => {
                return Err(WinApiControlSignal::WaitingForMessage.into());
            }
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetMessageA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!TranslateMessage`.
pub fn handle_translate_message(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let message_address = engine
        .read_rcx()
        .context("failed to read RCX for TranslateMessage")?;

    let translated = if message_address == 0 {
        false
    } else {
        let message_field_address = checked_field_address(message_address, 8, "MSG.message")?;

        let message = crate::guest_memory::read_u32(engine, message_field_address)
            .context("failed to read MSG.message for TranslateMessage")?;

        matches!(
            message,
            WM_KEYDOWN
                | WM_KEYUP
                | WM_CHAR
                | WM_DEADCHAR
                | WM_SYSKEYDOWN
                | WM_SYSKEYUP
                | WM_SYSCHAR
                | WM_SYSDEADCHAR
        )
    };

    let return_value = u64::from(translated);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from TranslateMessage")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub(crate) fn handle_default_window_procedure(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // hwnd / msg / wParam / lParam (and optional extra args) are ignored.
    // Returning 0 is the usual default for unhandled messages in stubs.
    let return_address = engine
        .return_from_win64_api(0)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `USER32.dll!DefWindowProcA`.
pub fn handle_def_window_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefWindowProcA")
}
/// Handles `USER32.dll!DefWindowProcW`.
pub fn handle_def_window_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefWindowProcW")
}
/// Handles `USER32.dll!DefFrameProcA`.
pub fn handle_def_frame_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefFrameProcA")
}
/// Handles `USER32.dll!DefFrameProcW`.
pub fn handle_def_frame_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefFrameProcW")
}
/// Handles `USER32.dll!DefMDIChildProcA`.
pub fn handle_def_mdi_child_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefMDIChildProcA")
}
/// Handles `USER32.dll!DefMDIChildProcW`.
pub fn handle_def_mdi_child_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefMDIChildProcW")
}
/// Handles `USER32.dll!DispatchMessageA`.
pub fn handle_dispatch_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let message_address = engine
        .read_rcx()
        .context("failed to read RCX for DispatchMessageA")?;

    if message_address == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from DispatchMessageA")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    /*
     * MSG on Win64:
     *
     * +0x00 HWND   hwnd
     * +0x08 UINT   message
     * +0x10 WPARAM wParam
     * +0x18 LPARAM lParam
     * +0x20 DWORD  time
     * +0x24 POINT  pt
     */

    let window_handle = read_guest_u64(engine, message_address)
        .context("failed to read MSG.hwnd for DispatchMessageA")?;

    let message_field_address = checked_field_address(message_address, 8, "MSG.message")?;

    let message = read_guest_u32(engine, message_field_address)
        .context("failed to read MSG.message for DispatchMessageA")?;

    let word_parameter_address = checked_field_address(message_address, 16, "MSG.wParam")?;

    let word_parameter = read_guest_u64(engine, word_parameter_address)
        .context("failed to read MSG.wParam for DispatchMessageA")?;

    let long_parameter_address = checked_field_address(message_address, 24, "MSG.lParam")?;

    let long_parameter = read_guest_u64(engine, long_parameter_address)
        .context("failed to read MSG.lParam for DispatchMessageA")?;

    let target_window = state
        .window_state
        .windows
        .iter()
        .find(|window| window.handle == window_handle);

    /*
     * Thread messages have hwnd == NULL and therefore no target WndProc.
     * Runtime-owned system controls currently also have window_proc == 0.
     * Both cases retain the old neutral DispatchMessageA behavior.
     */
    let Some(target_window) = target_window else {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from DispatchMessageA")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    };

    if target_window.window_proc == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from DispatchMessageA")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    /*
     * Do not return from DispatchMessageA yet.
     *
     * RIP and RSP remain at the fake DispatchMessageA entry, while the
     * runtime prepares to execute the guest WndProc. A later callback bridge
     * will invoke the callback and finally complete DispatchMessageA with the
     * callback's LRESULT.
     */
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: target_window.window_proc,
            window_handle,
            message,
            word_parameter,
            long_parameter,
            unicode: target_window.unicode,
        },
    }
    .into())
}
