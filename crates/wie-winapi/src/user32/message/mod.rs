//! The message pump core: Peek/Get/Send/Dispatch, Translate, Def*Proc, and
//! Post* (split from the former `message.rs`).
//!
//! Filter/synthesis helpers live in [`synth`]; the class-registry handlers
//! (RegisterClass/UnregisterClass/...) live in [`class`].

use super::SC_CLOSE;
use super::{
    Context, GuestCallbackRequest, HandlerContext, MessageQueueIdlePolicy, Msg,
    QueuedWindowMessage, Result, WM_CHAR, WM_CLOSE, WM_DEADCHAR, WM_DESTROY, WM_ERASEBKGND,
    WM_GETFONT, WM_KEYDOWN, WM_KEYUP, WM_MDICREATE, WM_PAINT, WM_QUIT, WM_SETFONT, WM_SYSCHAR,
    WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WinApiControlSignal, WinApiHandlerResult,
    WinApiState, WinMsg, create_mdi_child_from_struct, dispatch_control_proc, find_window,
    find_window_mut, is_known_window, read_u32, with_typed_read, write_message_structure,
};
use crate::OuterReturn;
use crate::gdi32::{ArgReg, read_arg};
use crate::kernel32::low_u32;
use crate::state::WindowFlags;

mod class;
mod synth;

pub use class::{
    handle_get_window_dc, handle_register_class_a, handle_register_class_w,
    handle_set_window_long_a, handle_set_window_long_w, handle_unregister_class_a,
    handle_unregister_class_w, handle_validate_rect,
};
pub(crate) use synth::erase_window_background;
use synth::{message_matches_filter, retarget_keyboard_messages, synthesize_idle_messages};

/// Handles `USER32.dll!PeekMessageA`.
pub fn handle_peek_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let message_address = read_arg(engine, ArgReg::Rcx, "PeekMessageA")?;

    let window_filter = read_arg(engine, ArgReg::Rdx, "PeekMessageA")?;

    let minimum_message_raw = read_arg(engine, ArgReg::R8, "PeekMessageA")?;

    let maximum_message_raw = read_arg(engine, ArgReg::R9, "PeekMessageA")?;

    let w_remove_msg = engine
        .read_rsp()
        .ok()
        .and_then(|rsp| read_u32(engine, rsp.wrapping_add(0x28)).ok())
        .unwrap_or(0);

    let minimum_message = low_u32(minimum_message_raw, "PeekMessageA minimum message")?;

    let maximum_message = low_u32(maximum_message_raw, "PeekMessageA maximum message")?;

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
            return ctx.finish(0);
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
        tracing::debug!(
            target: "wiegui",
            message = queued.message,
            hwnd = queued.window_handle.as_u64(),
            "PeekMessage: returning synthesized message"
        );
        write_message_structure(engine, message_address, &queued)?;
        1
    } else {
        // Queue-empty quiescence for PeekMessage-driven loops. SDL-style games
        // pump PeekMessageW and never park on GetMessage, so the pump's
        // WaitingForMessage drain never fires for them — without this, every
        // coalesced frame stays unpublished and the host window shows its
        // initial fill forever. Same semantics as that drain: one frame per
        // full repaint cycle, then the pull-half reconcile.
        state.present().drain_pending_publishes();
        state.present().reconcile_and_publish();
        0
    };

    ctx.finish(return_value)
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
    let _msg_va = read_arg(engine, ArgReg::Rcx, api_name)?;
    let _code = read_arg(engine, ArgReg::Rdx, api_name)?;

    ctx.finish(0)
}
/// Handles `USER32.dll!PostMessageA`.
pub fn handle_post_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = read_arg(engine, ArgReg::Rcx, "PostMessageA")?;

    let message_raw = read_arg(engine, ArgReg::Rdx, "PostMessageA")?;

    let word_parameter = read_arg(engine, ArgReg::R8, "PostMessageA")?;

    let long_parameter = read_arg(engine, ArgReg::R9, "PostMessageA")?;

    let message = low_u32(message_raw, "PostMessageA message")?;

    // HWND_BROADCAST (0xFFFF) and thread messages (NULL=0) are not yet
    // supported, so the gate is intentionally narrower than real Windows.
    let valid_window = is_known_window(state, window_handle);

    if valid_window {
        let mut queue = state.lock_message_queue();
        queue.push(
            crate::handles::Hwnd::from(window_handle),
            message,
            word_parameter,
            long_parameter,
        )?;
    }

    let return_value = u64::from(valid_window);

    ctx.finish(return_value)
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
    let window_handle = read_arg(engine, ArgReg::Rcx, api_name)?;

    let message_raw = read_arg(engine, ArgReg::Rdx, api_name)?;

    let word_parameter = read_arg(engine, ArgReg::R8, api_name)?;

    let long_parameter = read_arg(engine, ArgReg::R9, api_name)?;

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
        return ctx.finish(u64::from(erased));
    }

    // MDI client windows have no guest WndProc; WM_MDICREATE is handled here.
    if message == WM_MDICREATE {
        let child = create_mdi_child_from_struct(engine, state, long_parameter, prefer_unicode)
            .with_context(|| format!("failed to handle WM_MDICREATE in {api_name}"))?;

        return ctx.finish(child);
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
        return ctx.finish(result);
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

    // A window with no WndProc at all (no guest proc, not a control, not a
    // dialog): DefWindowProc semantics for the font messages so WM_SETFONT
    // still stores the font and WM_GETFONT still returns it.
    if message == WM_SETFONT {
        super::window::set_window_font(state, window_handle, word_parameter, long_parameter);
        return ctx.finish(0);
    }
    if message == WM_GETFONT {
        let font = super::window::window_font(state, window_handle);
        return ctx.finish(font);
    }

    ctx.finish(0)
}
/// Handles `USER32.dll!CallNextHookEx`.
pub fn handle_call_next_hook_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hook_handle = read_arg(engine, ArgReg::Rcx, "CallNextHookEx")?;

    let _code = read_arg(engine, ArgReg::Rdx, "CallNextHookEx")?;

    let _word_parameter = read_arg(engine, ArgReg::R8, "CallNextHookEx")?;

    let _long_parameter = read_arg(engine, ArgReg::R9, "CallNextHookEx")?;

    // There is currently no host-side hook chain after the guest hook.
    let return_value = 0;

    ctx.finish(return_value)
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
            queue.push(crate::handles::Hwnd::NULL, WM_QUIT, 0, 0)?;
            // `push` appends, but this synthetic WM_QUIT is returned directly
            // to the guest (never left in the queue) — take it back out.
            let quit_message = queue
                .messages
                .pop()
                .context("synthesized WM_QUIT vanished")?;
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
    let message_address = read_arg(engine, ArgReg::Rcx, "GetMessageA")?;

    let window_filter = read_arg(engine, ArgReg::Rdx, "GetMessageA")?;

    let minimum_message_raw = read_arg(engine, ArgReg::R8, "GetMessageA")?;

    let maximum_message_raw = read_arg(engine, ArgReg::R9, "GetMessageA")?;

    let minimum_message = low_u32(minimum_message_raw, "GetMessageA minimum message")?;

    let maximum_message = low_u32(maximum_message_raw, "GetMessageA maximum message")?;

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

    ctx.finish(return_value)
}
/// Handles `USER32.dll!TranslateMessage`.
pub fn handle_translate_message(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let message_address = read_arg(engine, ArgReg::Rcx, "TranslateMessage")?;

    let translated = if message_address == 0 {
        false
    } else {
        // One shared-lock borrow instead of a per-field read; the MSG layout
        // lives in `crate::guest_layout::Msg` (message @8).
        let message = with_typed_read::<Msg, _, _>(engine, message_address, |msg| Ok(msg.message))
            .context("failed to read MSG for TranslateMessage")?;

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

    ctx.finish(return_value)
}
pub(crate) fn handle_default_window_procedure(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let hwnd = read_arg(engine, ArgReg::Rcx, "DefWindowProc")?;
    let message_raw = read_arg(engine, ArgReg::Rdx, "DefWindowProc")?;
    let msg = low_u32(message_raw, "DefWindowProc message")?;
    let wparam = read_arg(engine, ArgReg::R8, "DefWindowProc")?;
    let lparam = read_arg(engine, ArgReg::R9, "DefWindowProc")?;

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
        WinMsg::WM_SETFONT => {
            // Store the HFONT on the window; a non-zero redraw flag (lparam)
            // invalidates it so the next repaint uses the new font.
            super::window::set_window_font(state, hwnd, wparam, lparam);
            0
        }
        WinMsg::WM_GETFONT => super::window::window_font(state, hwnd),
        WinMsg::WM_PAINT => {
            // F2 no-black: DefWindowProc validates the window, but it must
            // also honor the pending erase — real Windows runs WM_ERASEBKGND
            // before WM_PAINT, and when the guest swallowed the erase
            // (RNotepad returns 1 from WM_ERASEBKGND without erasing) the
            // class brush never fired. Without this, a zero-initialized DIB
            // publishes unpainted black. The erase fills the client with the
            // class-brush color (or the system background color when the
            // class has no brush); a later guest repaint overwrites it.
            let erase_pending = find_window(state, hwnd)
                .is_some_and(|window| window.flags.contains(WindowFlags::ERASE_BACKGROUND));
            if erase_pending && !erase_window_background(state, hwnd) {
                erase_with_system_background(state, hwnd);
            }
            if let Some(window) = find_window_mut(state, hwnd) {
                window.invalidated = false;
                // `erase_window_background` consumes the flag when it filled;
                // clear it here for the no-brush fallback (nothing to erase).
                window.flags.remove(WindowFlags::ERASE_BACKGROUND);
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

    ctx.finish(return_value)
}

/// F2 no-black fallback: fill `hwnd`'s background with the system default
/// (`COLOR_WINDOW`) when its class has no brush.
///
/// `erase_window_background` no-ops for brush-less classes (it returns
/// `false`, matching DefWindowProc's WM_ERASEBKGND result of 0), but the
/// WM_PAINT path must still prevent a zero-initialized DIB from publishing
/// unpainted black. The fill covers the window's rect inside the owning
/// surface; like the class-brush erase, the color is recorded as the
/// surface's background so the presenter clears with it.
fn erase_with_system_background(state: &mut WinApiState, hwnd: u64) {
    let Some(info) = crate::gdi32::resolve_window_ancestor(state, hwnd) else {
        return;
    };
    let (width, height) =
        find_window(state, hwnd).map_or((0, 0), |window| (window.width, window.height));
    if width <= 0 || height <= 0 {
        return;
    }
    let color = crate::user32::sys_color(crate::user32::COLOR_WINDOW); // white
    crate::gdi32::fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        width,
        height,
        color,
    );
    state.present().set_background_color(info.hwnd, color);
}

/// Handles `USER32.dll!DefWindowProcA`.
pub fn handle_def_window_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DefWindowProcW`.
pub fn handle_def_window_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DefFrameProcA`.
pub fn handle_def_frame_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DefFrameProcW`.
pub fn handle_def_frame_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DefMDIChildProcA`.
pub fn handle_def_mdi_child_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DefMDIChildProcW`.
pub fn handle_def_mdi_child_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx)
}
/// Handles `USER32.dll!DispatchMessageA`.
pub fn handle_dispatch_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let message_address = read_arg(engine, ArgReg::Rcx, "DispatchMessageA")?;

    if message_address == 0 {
        return ctx.finish(0);
    }

    // One shared-lock borrow instead of four per-field reads; the MSG layout
    // + pinned offsets live in `crate::guest_layout::Msg` (hwnd @0, message
    // @8, wParam @16, lParam @24).
    let (window_handle, message, word_parameter, long_parameter) =
        with_typed_read::<Msg, _, _>(engine, message_address, |msg| {
            Ok((msg.hwnd, msg.message, msg.wparam, msg.lparam))
        })
        .context("failed to read MSG for DispatchMessageA")?;

    // WM_ERASEBKGND is handled host-side (DefWindowProc semantics) — the
    // class-brush fill happens here and DispatchMessage returns TRUE, exactly
    // what a guest WndProc passing the message to DefWindowProc would report.
    if message == WM_ERASEBKGND {
        let erased = erase_window_background(state, window_handle);
        let return_value = u64::from(erased);
        return ctx.finish(return_value);
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
        return ctx.finish(0);
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
            return ctx.finish(result);
        }

        // Modal dialogs have no guest WndProc but a dialog proc. WM_PAINT
        // paints the dialog face into the owner surface; everything else
        // (WM_COMMAND, WM_CLOSE, ...) bridges to the dialog proc. Host-owned
        // modeless dialogs (comdlg32 Find/Replace) have no dialog proc — they
        // paint their face the same way, and their buttons are handled at the
        // control level (`deliver_button_command`), so other messages fall
        // through to the neutral zero below.
        let is_host_find_dialog = crate::comdlg32::is_find_dialog_window(state, window_handle);
        if dialog_proc != 0 || is_host_find_dialog {
            if message == WM_PAINT {
                super::dialog::paint_dialog(state, window_handle);
            } else if dialog_proc != 0 {
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

        return ctx.finish(0);
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
    let exit_code = read_arg(engine, ArgReg::Rcx, "PostQuitMessage")?;

    tracing::info!(
        target: "wiegui",
        code = exit_code,
        "PostQuitMessage"
    );

    let mut queue = state.lock_message_queue();
    // `push` stamps the message and broadcasts a MessagePosted wake token to
    // any parked GetMessage (Painpoint 1).
    queue.push(crate::handles::Hwnd::NULL, WM_QUIT, exit_code, 0)?;

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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::handle_default_window_procedure;
    use crate::guest_heap::GuestHeap;
    use crate::handles::Hwnd;
    use crate::present::MessageQueue;
    use crate::state::{
        FileIoState, HeapState, KernelState as KernelStateT, ModuleState, ProcessState,
        WinApiEnvironment, WindowClassRecord, WindowFlags,
    };
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::{CreateWindowRequest, WindowClassIdentifier, find_window_mut};
    use crate::vfs::VolumeConfig;
    use crate::{DEFAULT_ENVIRONMENT, DllStateMap, HandlerContext, WinApiState, present};
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    /// Minimal engine for handler tests: guest pages + a return address.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, wie_cpu::RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    fn test_env() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    fn test_state() -> WinApiState {
        WinApiState {
            display: crate::DisplayMetrics::default(),
            heap_state: HeapState {
                heap: std::sync::Arc::new(std::sync::Mutex::new(GuestHeap::new(0x2000, 0x10000))),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelStateT {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    /// Register a class with the classic `(HBRUSH)(COLOR_x + 1)` stock brush
    /// (`brush`; 6 = COLOR_WINDOW white, 7 = COLOR_WINDOWTEXT black) and
    /// create a shown top-level window from it — notepad's main-window
    /// pattern. Returns the HWND.
    fn push_brush_window(
        state: &mut WinApiState,
        class_name: &str,
        brush: u64,
        width: i32,
        height: i32,
    ) -> u64 {
        let atom = crate::user32::register_window_class(
            state,
            WindowClassRecord {
                atom: 0,
                class_name: class_name.to_owned(),
                window_proc: 0x7000_0000,
                style: 0,
                instance_handle: 0,
                icon_handle: 0,
                cursor_handle: 0,
                background_brush: brush,
                small_icon_handle: 0,
                menu_name: 0,
                unicode: true,
            },
        )
        .expect("register class");
        let (hwnd, _, _) = crate::user32::create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Name(class_name.to_owned()),
                title: "Test".to_owned(),
                style: 0,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 0,
                instance_handle: 0,
                x: 0,
                y: 0,
                width,
                height,
            },
            true,
        )
        .expect("create window");
        assert_ne!(atom, 0, "class registration must succeed");
        if let Some(window) = find_window_mut(state, hwnd) {
            window.visible = true;
        }
        hwnd
    }

    /// Mark `hwnd` invalidated with a pending erase — what a resize
    /// (`InvalidateRect` bErase, `SetWindowPlacement`, ...) leaves behind.
    fn mark_erase_pending(state: &mut WinApiState, hwnd: u64) {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }

    /// Drive DefWindowProc with `WM_PAINT` exactly like the guest's dispatch
    /// would after a WndProc fall-through.
    fn dispatch_wm_paint(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64) {
        let message = u64::from(crate::user32::WinMsg::WM_PAINT.as_u32());
        write_regs(engine, hwnd, message, 0, 0);
        handle_default_window_procedure(&mut HandlerContext::new(engine, test_env(), state))
            .expect("DefWindowProc WM_PAINT");
    }

    /// An empty frame for the headless record slot.
    fn empty_record() -> present::SurfaceFrame {
        present::SurfaceFrame {
            width: 0,
            stride: 0,
            height: 0,
            pixels: Arc::new(Vec::new()),
            background_color: 0x00FF_FFFF,
            region: None,
        }
    }

    fn published_frame(state: &mut WinApiState, hwnd: u64) -> present::SurfaceFrame {
        state
            .present()
            .published
            .get(&Hwnd::from(hwnd))
            .cloned()
            .expect("published frame")
    }

    #[test]
    fn def_window_proc_paint_erases_pending_class_brush_background() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_brush_window(&mut state, "WhiteBrush", 6, 64, 32);
        mark_erase_pending(&mut state, hwnd);
        state.present().record = Some(Box::new(empty_record()));

        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        state.present().drain_pending_publishes();

        // F2: the erase filled the whole client with COLOR_WINDOW-white —
        // zero unpainted-black pixels in the published frame.
        let frame = published_frame(&mut state, hwnd);
        assert_eq!((frame.width, frame.height), (64, 32));
        assert_eq!(frame.background_color, 0x00FF_FFFF);
        assert!(
            !frame.pixels.contains(&0x0000_0000),
            "idle frame must contain no black pixels"
        );
        assert!(
            frame.pixels.iter().all(|&px| px == 0x00FF_FFFF),
            "the class-brush erase fills every pixel white"
        );
        let recorded = state.present().record.as_ref().expect("recorded frame");
        assert!(
            !recorded.pixels.contains(&0x0000_0000),
            "the headless record slot carries the same no-black frame"
        );
        // The erase consumed the pending-erase flag; a second WM_PAINT must
        // not re-erase (no double fill).
        let window = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == Hwnd::from(hwnd))
            .expect("window record");
        assert!(!window.flags.contains(WindowFlags::ERASE_BACKGROUND));
    }

    #[test]
    fn def_window_proc_paint_without_pending_erase_skips_fill() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_brush_window(&mut state, "NoEraseClass", 6, 64, 32);
        // No ERASE_BACKGROUND: the window was already erased or never
        // invalidated — DefWindowProc must only validate, not fill.
        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        assert_eq!(state.present().drain_pending_publishes(), 0);
        assert!(state.present().surfaces.is_empty(), "no erase, no surface");
    }

    #[test]
    fn def_window_proc_paint_erases_brushless_window_with_system_background() {
        let mut engine = test_engine();
        let mut state = test_state();
        // An unregistered class name → no class brush (class_atom 0).
        let (hwnd, _, _) = crate::user32::create_window_record(
            &mut state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Name("Brushless".to_owned()),
                title: "Brushless".to_owned(),
                style: 0,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 0,
                instance_handle: 0,
                x: 0,
                y: 0,
                width: 48,
                height: 24,
            },
            true,
        )
        .expect("create window");
        mark_erase_pending(&mut state, hwnd);

        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        state.present().drain_pending_publishes();

        // F2 fallback: no class brush → fill with the system background
        // (COLOR_WINDOW white) so the DIB still never publishes black.
        let frame = published_frame(&mut state, hwnd);
        assert_eq!(frame.background_color, 0x00FF_FFFF);
        // Scan only the LOGICAL pixels: the 64-padded row pitch (ADR-0001)
        // keeps the padding tail zeroed, and those padding slots are not
        // frame content.
        let no_black = (0..frame.height)
            .all(|y| (0..frame.width).all(|x| frame.pixel(x, y) != Some(0x0000_0000)));
        assert!(
            no_black,
            "brush-less windows still erase to the system background"
        );
    }

    #[test]
    fn resize_then_idle_frame_is_fully_painted() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_brush_window(&mut state, "ResizeClass", 6, 64, 32);
        mark_erase_pending(&mut state, hwnd);
        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        state.present().drain_pending_publishes();
        let first = published_frame(&mut state, hwnd);
        assert_eq!((first.width, first.height), (64, 32));
        assert!(!first.pixels.contains(&0x0000_0000));

        // Resize: the window grows, the DIB reallocates zeroed (black until
        // repainted), and the geometry path leaves an erase pending.
        if let Some(window) = find_window_mut(&mut state, hwnd) {
            window.width = 128;
            window.height = 64;
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }

        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        state.present().drain_pending_publishes();

        // The resize-then-idle frame is fully painted at the new size — no
        // black seams from the zeroed realloc.
        let resized = published_frame(&mut state, hwnd);
        assert_eq!((resized.width, resized.height), (128, 64));
        assert!(
            !resized.pixels.contains(&0x0000_0000),
            "resize-then-idle frame must be fully painted"
        );
        assert!(
            resized.pixels.iter().all(|&px| px == 0x00FF_FFFF),
            "every pixel is the erased background after the resize"
        );
    }

    #[test]
    fn black_class_brush_erases_black_and_records_it() {
        let mut engine = test_engine();
        let mut state = test_state();
        // brush 7 = (HBRUSH)(COLOR_WINDOWTEXT + 1) → genuinely black.
        let hwnd = push_brush_window(&mut state, "BlackClass", 7, 16, 16);
        mark_erase_pending(&mut state, hwnd);

        dispatch_wm_paint(&mut engine, &mut state, hwnd);
        state.present().drain_pending_publishes();

        // This is the invariant's "legitimately black content" case: the
        // erase honors the window's own brush, and the frame records the
        // black background so the presenter clears with it (not white).
        let frame = published_frame(&mut state, hwnd);
        assert_eq!(frame.background_color, 0x0000_0000);
        assert!(
            frame.pixels.iter().all(|&px| px == 0x0000_0000),
            "a black-brush window is legitimately black"
        );
    }
}
