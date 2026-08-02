//! The message pump core: Peek/Get/Send/Dispatch, Translate, Def*Proc, and
//! Post* (split from the former `message.rs`).
//!
//! Filter/synthesis helpers live in [`synth`]; the class-registry handlers
//! (RegisterClass/UnregisterClass/...) live in [`class`].

use super::SC_CLOSE;
use super::{
    Context, GuestCallbackRequest, HandlerContext, MessageQueueIdlePolicy, QueuedWindowMessage,
    Result, WM_CHAR, WM_CLOSE, WM_DEADCHAR, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP,
    WM_MDICREATE, WM_PAINT, WM_QUIT, WM_SYSCHAR, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WinMsg, checked_field_address,
    create_mdi_child_from_struct, dispatch_control_proc, find_window_mut, is_known_window,
    read_guest_u32, read_guest_u64, write_message_structure,
};
use crate::OuterReturn;

mod class;
mod synth;

pub use class::{
    handle_get_window_dc, handle_register_class_a, handle_register_class_w,
    handle_set_window_long_a, handle_set_window_long_w, handle_unregister_class_a,
    handle_unregister_class_w, handle_validate_rect,
};
pub(crate) use synth::erase_window_background;
use synth::{
    message_matches_filter, note_wm_quit_consumed, retarget_keyboard_messages,
    synthesize_idle_messages,
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

    // Keyboard messages go to the focus window; rewrite before the filter scan
    // so the filter and the returned MSG both carry the effective target.
    retarget_keyboard_messages(state);

    let matching_index = {
        let queue = state.lock_message_queue();
        queue.messages.iter().position(|queued| {
            message_matches_filter(
                state,
                window_filter,
                minimum_message,
                maximum_message,
                queued,
            )
        })
    };

    let return_value = if let Some(index) = matching_index {
        let queued = if w_remove_msg != 0 {
            // PM_REMOVE: remove from queue.
            let mut queue = state.lock_message_queue();
            queue.messages.remove(index)
        } else {
            // PM_NOREMOVE: leave in queue. Index is from `position` on this Vec.
            let queue = state.lock_message_queue();
            queue
                .messages
                .get(index)
                .cloned()
                .context("PeekMessageA matching index vanished")?
        };
        if w_remove_msg != 0 && queued.message == WM_QUIT {
            note_wm_quit_consumed(state);
        }
        tracing::debug!(
            target: "wiegui",
            message = queued.message,
            hwnd = queued.window_handle.as_u64(),
            "PeekMessage: returning message"
        );
        write_message_structure(engine, message_address, &queued)?;
        1
    } else if synthesize_idle_messages(state, window_filter, minimum_message, maximum_message)? {
        // A WM_TIMER / WM_PAINT was queued; return it on this same call.
        let mut queue = state.lock_message_queue();
        let synthesized_index = queue.messages.iter().position(|queued| {
            message_matches_filter(
                state,
                window_filter,
                minimum_message,
                maximum_message,
                queued,
            )
        });
        let Some(synthesized_index) = synthesized_index else {
            drop(queue);
            return Ok(WinApiHandlerResult {
                return_address: engine
                    .return_from_win64_api(0)
                    .context("failed to return from PeekMessageA")?,
                return_value: 0,
            });
        };
        let queued = if w_remove_msg != 0 {
            // PM_REMOVE: remove from queue.
            queue.messages.remove(synthesized_index)
        } else {
            // PM_NOREMOVE: leave in queue.
            queue
                .messages
                .get(synthesized_index)
                .cloned()
                .context("PeekMessageA synthesized index vanished")?
        };
        drop(queue);
        if w_remove_msg != 0 && queued.message == WM_QUIT {
            note_wm_quit_consumed(state);
        }
        tracing::debug!(
            target: "wiegui",
            message = queued.message,
            hwnd = queued.window_handle.as_u64(),
            "PeekMessage: returning synthesized message"
        );
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

    // HWND_BROADCAST (0xFFFF) and thread messages (NULL=0) are not yet
    // supported, so the gate is intentionally narrower than real Windows.
    let valid_window = is_known_window(state, window_handle);

    if valid_window {
        let mut queue = state.lock_message_queue();
        let time = queue.next_message_time;
        queue.next_message_time = queue
            .next_message_time
            .checked_add(1)
            .context("PostMessageA timestamp overflow")?;
        queue.messages.push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(window_handle),
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

    tracing::debug!(
        target: "wiegui",
        api = %api_name,
        message,
        hwnd = window_handle,
        "SendMessage"
    );

    // WM_ERASEBKGND is handled host-side (DefWindowProc semantics): fill the
    // class-brush background without invoking a guest WndProc.
    if message == WM_ERASEBKGND {
        let erased = erase_window_background(state, window_handle);
        let return_address = engine
            .return_from_win64_api(u64::from(erased))
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: u64::from(erased),
        });
    }

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
                outer_return: OuterReturn::Passthrough,
            },
        }
        .into());
    }

    // Built-in controls have no guest WndProc; route to the host-side control
    // WndProc instead of returning the neutral zero.
    let is_control = super::find_window(state, window_handle)
        .is_some_and(|window| window.control_kind.is_some());
    if is_control
        && let Some(result) = dispatch_control_proc(
            engine,
            state,
            window_handle,
            message,
            word_parameter,
            long_parameter,
        )?
    {
        let return_address = engine
            .return_from_win64_api(result)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: result,
        });
    }

    // Modal dialogs have a dialog proc instead of a guest WndProc; bridge
    // synchronous sends (e.g. WM_COMMAND from a timer) straight to it.
    if let Some(dialog_window) = super::find_window(state, window_handle)
        && dialog_window.dialog_proc != 0
    {
        let dialog_proc = dialog_window.dialog_proc;
        let dialog_unicode = dialog_window.dialog_unicode;
        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: dialog_proc,
                window_handle,
                message,
                word_parameter,
                long_parameter,
                unicode: dialog_unicode,
                outer_return: OuterReturn::Passthrough,
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
/// Handle an empty message queue in `GetMessageA/W`.
///
/// `ExitOnIdle` synthesizes a `WM_QUIT` so the guest performs its normal
/// teardown; `YieldOnIdle` yields back to the runtime (`WaitingForMessage`).
fn empty_queue_result(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    message_address: u64,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    // A modal dialog is open: never synthesize the regression-mode WM_QUIT —
    // it would close the dialog the instant it opens. Yield so the runtime can
    // keep the guest alive (the dialog's own EndDialog posts the real quit).
    if state.lock_message_queue().dialog_depth > 0 {
        return Err(WinApiControlSignal::WaitingForMessage.into());
    }

    let return_value = match state.window_state().message_queue_idle_policy {
        MessageQueueIdlePolicy::ExitOnIdle => {
            /*
             * Regression mode: represent an empty queue as a synthetic
             * WM_QUIT so the guest performs its normal teardown.
             */
            let mut queue = state.lock_message_queue();
            let quit_message = QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::NULL,
                message: WM_QUIT,
                word_parameter: 0,
                long_parameter: 0,
                time: queue.next_message_time,
                point_x: 0,
                point_y: 0,
            };

            queue.next_message_time = queue
                .next_message_time
                .checked_add(1)
                .context("GetMessageA timestamp overflow")?;

            drop(queue);
            write_message_structure(engine, message_address, &quit_message)?;

            0
        }

        MessageQueueIdlePolicy::YieldOnIdle => {
            return Err(WinApiControlSignal::WaitingForMessage.into());
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

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

    // Keyboard messages go to the focus window; rewrite before the filter scan
    // so the filter and the returned MSG both carry the effective target.
    retarget_keyboard_messages(state);

    let matching_index = {
        let queue = state.lock_message_queue();
        queue.messages.iter().position(|queued| {
            message_matches_filter(
                state,
                window_filter,
                minimum_message,
                maximum_message,
                queued,
            )
        })
    };

    let return_value = if message_address == 0 {
        // GetMessage returns -1 on failure.
        u64::from(u32::MAX)
    } else if let Some(index) = matching_index {
        let mut queue = state.lock_message_queue();
        let queued = queue.messages.remove(index);
        drop(queue);
        if queued.message == WM_QUIT {
            note_wm_quit_consumed(state);
        }

        tracing::debug!(
            target: "wiegui",
            message = queued.message,
            hwnd = queued.window_handle.as_u64(),
            "GetMessage: returning message"
        );
        write_message_structure(engine, message_address, &queued)?;

        u64::from(queued.message != WM_QUIT)
    } else if synthesize_idle_messages(state, window_filter, minimum_message, maximum_message)? {
        // A WM_TIMER / WM_PAINT was queued; re-scan once and return it.
        let mut queue = state.lock_message_queue();
        let Some(synthesized_index) = queue.messages.iter().position(|queued| {
            message_matches_filter(
                state,
                window_filter,
                minimum_message,
                maximum_message,
                queued,
            )
        }) else {
            drop(queue);
            // The synthesized message matched by construction; if it vanished
            // anyway, fall through to the empty-queue policy.
            return empty_queue_result(engine, state, message_address, "GetMessageA");
        };
        let queued = queue.messages.remove(synthesized_index);
        drop(queue);
        if queued.message == WM_QUIT {
            note_wm_quit_consumed(state);
        }
        tracing::debug!(
            target: "wiegui",
            message = queued.message,
            hwnd = queued.window_handle.as_u64(),
            "GetMessage: returning synthesized message"
        );
        write_message_structure(engine, message_address, &queued)?;

        u64::from(queued.message != WM_QUIT)
    } else {
        return empty_queue_result(engine, state, message_address, "GetMessageA");
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
        let message_field_address = checked_field_address(message_address, 8, "MSG.message");

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
    let state = &mut *ctx.state;

    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for DefWindowProc")?;
    let message_raw = engine
        .read_rdx()
        .context("failed to read RDX for DefWindowProc")?;
    let msg = u32::try_from(message_raw & u64::from(u32::MAX))
        .context("DefWindowProc message does not fit u32")?;
    let wparam = engine
        .read_r8()
        .context("failed to read R8 for DefWindowProc")?;
    let _lparam = engine
        .read_r9()
        .context("failed to read R9 for DefWindowProc")?;

    tracing::trace!(
        target: "wiegui",
        hwnd,
        message = msg,
        "DefWindowProc fallthrough"
    );

    let return_value: u64 = match WinMsg::from(msg) {
        WinMsg::WM_CLOSE => {
            // Post WM_DESTROY to self
            tracing::debug!(
                target: "wiegui",
                hwnd,
                "DefWindowProc: WM_CLOSE -> WM_DESTROY"
            );
            let mut queue = state.lock_message_queue();
            let time = queue.next_message_time;
            queue.next_message_time = time
                .checked_add(1)
                .context("DefWindowProc: message time overflow")?;
            queue.messages.push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(hwnd),
                message: WM_DESTROY,
                word_parameter: 0,
                long_parameter: 0,
                time,
                point_x: 0,
                point_y: 0,
            });
            0
        }
        WinMsg::WM_ERASEBKGND => {
            // DefWindowProc fills the class-brush background and returns
            // nonzero when a brush erased it (host-side, no guest callback).
            u64::from(erase_window_background(state, hwnd))
        }
        WinMsg::WM_SETCURSOR => {
            // Cursor handled: unconditional success.
            1
        }
        WinMsg::WM_PAINT => {
            // Validate the window to prevent livelock
            if let Some(window) = find_window_mut(state, hwnd) {
                window.invalidated = false;
                // No BeginPaint ran: the pending erase has no consumer.
                window.erase_background = false;
            }
            0
        }
        WinMsg::WM_SYSCOMMAND if wparam == SC_CLOSE => {
            // Same as WM_CLOSE
            let mut queue = state.lock_message_queue();
            let time = queue.next_message_time;
            queue.next_message_time = time
                .checked_add(1)
                .context("DefWindowProc: message time overflow")?;
            queue.messages.push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(hwnd),
                message: WM_CLOSE,
                word_parameter: 0,
                long_parameter: 0,
                time,
                point_x: 0,
                point_y: 0,
            });
            0
        }
        // WM_NCCALCSIZE (no non-client area) and everything else: no-op.
        _ => 0,
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
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

    let message_field_address = checked_field_address(message_address, 8, "MSG.message");

    let message = read_guest_u32(engine, message_field_address)
        .context("failed to read MSG.message for DispatchMessageA")?;

    let word_parameter_address = checked_field_address(message_address, 16, "MSG.wParam");

    let word_parameter = read_guest_u64(engine, word_parameter_address)
        .context("failed to read MSG.wParam for DispatchMessageA")?;

    let long_parameter_address = checked_field_address(message_address, 24, "MSG.lParam");

    let long_parameter = read_guest_u64(engine, long_parameter_address)
        .context("failed to read MSG.lParam for DispatchMessageA")?;

    // WM_ERASEBKGND is handled host-side (DefWindowProc semantics) — the
    // class-brush fill happens here and DispatchMessage returns TRUE, exactly
    // what a guest WndProc passing the message to DefWindowProc would report.
    if message == WM_ERASEBKGND {
        let erased = erase_window_background(state, window_handle);
        let return_value = u64::from(erased);
        let return_address = engine
            .return_from_win64_api(return_value)
            .context("failed to return from DispatchMessageA")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value,
        });
    }

    let target_window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(window_handle));

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

    // Extract the Copy fields so the record borrow ends before the control
    // dispatch (which needs the state mutably).
    let window_proc = target_window.window_proc;
    let control_kind = target_window.control_kind;
    let unicode = target_window.unicode;
    let dialog_proc = target_window.dialog_proc;
    let dialog_unicode = target_window.dialog_unicode;

    tracing::debug!(
        target: "wiegui",
        message,
        hwnd = window_handle,
        route = if window_proc != 0 {
            "wndproc"
        } else if control_kind.is_some() {
            "control"
        } else if dialog_proc != 0 {
            "dialog"
        } else {
            "none"
        },
        "DispatchMessage"
    );

    if window_proc == 0 {
        // Built-in controls have no guest WndProc; route to the host-side
        // control WndProc instead of returning the neutral zero.
        if control_kind.is_some()
            && let Some(result) = dispatch_control_proc(
                engine,
                state,
                window_handle,
                message,
                word_parameter,
                long_parameter,
            )?
        {
            let return_address = engine
                .return_from_win64_api(result)
                .context("failed to return from DispatchMessageA")?;
            return Ok(WinApiHandlerResult {
                return_address,
                return_value: result,
            });
        }

        // Modal dialogs have no guest WndProc but a dialog proc. WM_PAINT
        // paints the dialog face into the owner surface; everything else
        // (WM_COMMAND, WM_CLOSE, ...) bridges to the dialog proc.
        if dialog_proc != 0 {
            if message == WM_PAINT {
                super::dialog::paint_dialog(state, window_handle);
            } else {
                return Err(WinApiControlSignal::GuestCallbackRequested {
                    request: GuestCallbackRequest {
                        callback_address: dialog_proc,
                        window_handle,
                        message,
                        word_parameter,
                        long_parameter,
                        unicode: dialog_unicode,
                        outer_return: OuterReturn::Passthrough,
                    },
                }
                .into());
            }
        }

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
            callback_address: window_proc,
            window_handle,
            message,
            word_parameter,
            long_parameter,
            unicode,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}

/// Handles `USER32.dll!PostQuitMessage`.
pub fn handle_post_quit_message(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let exit_code = engine
        .read_rcx()
        .context("failed to read RCX for PostQuitMessage")?;

    tracing::info!(
        target: "wiegui",
        code = exit_code,
        "PostQuitMessage"
    );

    let mut queue = state.lock_message_queue();
    let time = queue.next_message_time;
    queue.next_message_time = time
        .checked_add(1)
        .context("PostQuitMessage: message time overflow")?;

    queue.messages.push(QueuedWindowMessage {
        window_handle: crate::handles::Hwnd::NULL,
        message: WM_QUIT,
        word_parameter: exit_code,
        long_parameter: 0,
        time,
        point_x: 0,
        point_y: 0,
    });

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from PostQuitMessage")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `USER32.dll!GetMessageW`.
pub fn handle_get_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_message_a(ctx)
}

/// Handles `USER32.dll!PeekMessageW`.
pub fn handle_peek_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_peek_message_a(ctx)
}

/// Handles `USER32.dll!DispatchMessageW`.
pub fn handle_dispatch_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_dispatch_message_a(ctx)
}

/// Handles `USER32.dll!PostMessageW`.
pub fn handle_post_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_post_message_a(ctx)
}
