//! Window-manager state: show/enable/focus, invalidation, update, destroy, and
//! the focus-change delivery (split from the former `window.rs`).

use super::class::{find_window, find_window_mut};
use crate::OuterReturn;
use crate::state::WindowFlags;
use crate::user32::{
    Context, FAKE_WINDOW_HANDLE, GWLP_WNDPROC_RAW, GuestCallbackRequest, HandlerContext, Result,
    WM_DESTROY, WM_KILLFOCUS, WM_PAINT, WM_SETFOCUS, WinApiControlSignal, WinApiHandlerResult,
    WinApiState, dispatch_control_proc, get_window_long_ptr_value, is_known_window,
};

/// Handles `USER32.dll!IsWindow`.
pub fn handle_is_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindow")?;

    // Any runtime-known window is a valid window — including child controls,
    // not just the legacy fake top-level handle.
    let return_value = u64::from(is_known_window(state, window_handle));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsWindowVisible`.
pub fn handle_is_window_visible(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowVisible")?;

    let return_value = u64::from(if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_visible
    } else {
        find_window(state, window_handle).is_some_and(|window| window.visible)
    });

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowVisible")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsWindowEnabled`.
pub fn handle_is_window_enabled(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowEnabled")?;

    let return_value = u64::from(if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_enabled
    } else {
        find_window(state, window_handle)
            .is_some_and(|window| window.flags.contains(WindowFlags::ENABLED))
    });

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowEnabled")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetParent`.
pub fn handle_get_parent(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetParent")?;

    let return_value =
        find_window(state, window_handle).map_or(0, |window| window.parent_handle.as_u64());

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetParent")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetActiveWindow`.
pub fn handle_get_active_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().active_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetForegroundWindow`.
pub fn handle_get_foreground_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().foreground_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetForegroundWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ShowWindow`.
pub fn handle_show_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ShowWindow")?;

    let show_command = engine
        .read_rdx()
        .context("failed to read RDX for ShowWindow")?;

    let previously_visible = state.window_state().window_visible;
    tracing::debug!(
        target: "wiegui",
        hwnd = window_handle,
        show_command,
        "ShowWindow"
    );
    // The parent of a window this call hides while it was visible: its rect
    // vacates the owner surface, so the OWNER must erase over it (see below).
    let mut hidden_child_parent = None;

    if window_handle == FAKE_WINDOW_HANDLE {
        // SW_HIDE is zero. Other commands make the window visible in the
        // current single-window model.
        state.window_state().window_visible = show_command != 0;
    } else if let Some(window) = find_window_mut(state, window_handle) {
        // Real window records track their own visibility (used by the host
        // mouse hit-test in `GuestHandle::window_at`).
        let was_visible = window.visible;
        if show_command == 0 && was_visible {
            hidden_child_parent = Some(window.parent_handle);
        }
        window.visible = show_command != 0;
        // Showing a window invalidates it with erase (real Windows): the
        // first paint cycle fills the client with the class-brush background
        // before the guest paints. Without this the owner surface stays
        // zeroed (black) until something else triggers an erase — a modal
        // dialog close, which then visibly "changes the background color".
        if show_command != 0 {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }

    if show_command != 0 {
        // Showing a window is a visible change: bump the content revision so
        // the idle reconcile republishes its first painted frame (an unknown
        // window resolves to nothing and is a silent no-op).
        crate::present::PresentState::request_paint(state, window_handle);
    } else if let Some(parent) = hidden_child_parent {
        // Hiding a visible CHILD vacates its rect in the owner surface —
        // real Windows repaints the parent's vacated region. Invalidate the
        // owner with erase and bump its revision, AND erase the owner
        // surface synchronously right now: the vacated rect is covered
        // without waiting for the paint synthesizer's cycle (the child is
        // hidden, so the WS_CLIPCHILDREN subtraction no longer excludes its
        // rect). The later synthesized erase is then a harmless repeat.
        if let Some(owner) = find_window_mut(state, parent.as_u64()) {
            owner.invalidated = true;
            owner.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
        crate::user32::message::erase_window_background(state, parent.as_u64());
        crate::present::PresentState::request_paint(state, parent.as_u64());
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
/// Handles `USER32.dll!EnableWindow`.
pub fn handle_enable_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for EnableWindow")?;

    let enable_raw = engine
        .read_rdx()
        .context("failed to read RDX for EnableWindow")?;

    let previously_disabled = !state.window_state().window_enabled;

    if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_enabled = enable_raw != 0;
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
/// Handles `USER32.dll!SetForegroundWindow`.
pub fn handle_set_foreground_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetForegroundWindow")?;

    let success = window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state().foreground_window_handle = crate::handles::Hwnd::from(window_handle);
        state.window_state().active_window_handle = crate::handles::Hwnd::from(window_handle);
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
/// Handles `USER32.dll!SetActiveWindow`.
pub fn handle_set_active_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetActiveWindow")?;

    let previous_window = state.window_state().active_window_handle;

    if window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().active_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    let return_address = engine
        .return_from_win64_api(previous_window.as_u64())
        .context("failed to return from SetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window.as_u64(),
    })
}
/// Handles `USER32.dll!SetFocus`.
pub fn handle_set_focus(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetFocus")?;

    let previous_window = state.window_state().focus_window_handle;
    let accepted = window_handle == 0 || is_known_window(state, window_handle);

    if accepted {
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    // Windows sends WM_KILLFOCUS(old, new) then WM_SETFOCUS(new, old) when
    // the focus actually moves. Bridge synchronously (guest WndProcs) or
    // host-side (controls) — see deliver_focus_change.
    if accepted
        && crate::handles::Hwnd::from(window_handle) != previous_window
        && let Some(signal) = deliver_focus_change(
            state,
            engine,
            previous_window.as_u64(),
            window_handle,
            OuterReturn::Fixed(previous_window.as_u64()),
        )?
    {
        return Err(signal.into());
    }

    let return_address = engine
        .return_from_win64_api(previous_window.as_u64())
        .context("failed to return from SetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window.as_u64(),
    })
}

/// Bridge one guest callback, or post it when the bridge is already taken.
///
/// The bridge is one-shot per API stop: the first callback returns as
/// `GuestCallbackRequested`; a later one is posted so it arrives after the
/// bridged message (queue order).
fn bridge_or_post(
    state: &mut WinApiState,
    bridged: &mut Option<WinApiControlSignal>,
    request: GuestCallbackRequest,
) -> Result<()> {
    if bridged.is_none() {
        *bridged = Some(WinApiControlSignal::GuestCallbackRequested { request });
        return Ok(());
    }
    state.lock_message_queue().push(
        crate::handles::Hwnd::from(request.window_handle),
        request.message,
        request.word_parameter,
        request.long_parameter,
    )
}

/// Deliver the focus-change message pair (`WM_KILLFOCUS` to `old_focus`, then
/// `WM_SETFOCUS` to `new_focus`) when the focus actually moved.
///
/// Control targets (host-side WndProc) are dispatched directly; a target with
/// a guest WndProc or dialog proc must be bridged through the
/// `GuestCallbackRequested` mechanism. The bridge is one-shot per API stop, so
/// at most one of the two is bridged — the other is dispatched host-side or
/// posted to the guest's queue (correct order, delivered before any input).
///
/// Returns the bridge signal when one is required (the caller returns it as
/// `Err(..)`); `Ok(None)` means both were delivered and the caller completes
/// normally.
pub(crate) fn deliver_focus_change(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    old_focus: u64,
    new_focus: u64,
    outer_return: OuterReturn,
) -> Result<Option<WinApiControlSignal>> {
    if old_focus == new_focus {
        return Ok(None);
    }

    // WM_KILLFOCUS first (Windows order), then WM_SETFOCUS. KILLFOCUS gets the
    // bridge slot when it needs one; a guest-WndProc SETFOCUS is then queued.
    let mut bridged = None;
    for (hwnd, message, wparam) in [
        (old_focus, WM_KILLFOCUS, new_focus),
        (new_focus, WM_SETFOCUS, old_focus),
    ] {
        if hwnd == 0 || !is_known_window(state, hwnd) {
            continue;
        }
        let (window_proc, dialog_proc, dialog_unicode, unicode, is_control) = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map_or((0, 0, false, false, false), |w| {
                (
                    w.window_proc,
                    w.dialog_proc,
                    w.dialog_unicode,
                    w.unicode,
                    w.control_kind.is_some(),
                )
            });
        if is_control {
            // A guest-subclassed control (GWLP_WNDPROC replaced by the guest)
            // must see the focus message through its subclass proc, exactly
            // like a guest WndProc window — the subclass forwards it through
            // CallWindowProcW back into the host default. Plain controls
            // update their focus state host-side.
            let subclass =
                get_window_long_ptr_value(hwnd, GWLP_WNDPROC_RAW, state, "deliver_focus_change")?;
            if subclass == 0 {
                let _ = dispatch_control_proc(engine, state, hwnd, message, wparam, 0)?;
                continue;
            }
            let request = GuestCallbackRequest {
                callback_address: subclass,
                window_handle: hwnd,
                message,
                word_parameter: wparam,
                long_parameter: 0,
                unicode,
                outer_return,
            };
            bridge_or_post(state, &mut bridged, request)?;
            continue;
        }
        if window_proc == 0 && dialog_proc == 0 {
            continue;
        }
        let callback_address = if window_proc != 0 {
            window_proc
        } else {
            dialog_proc
        };
        let callback_unicode = if window_proc != 0 {
            unicode
        } else {
            dialog_unicode
        };
        let request = GuestCallbackRequest {
            callback_address,
            window_handle: hwnd,
            message,
            word_parameter: wparam,
            long_parameter: 0,
            unicode: callback_unicode,
            outer_return,
        };
        bridge_or_post(state, &mut bridged, request)?;
    }
    Ok(bridged)
}
/// Handles `USER32.dll!GetFocus`.
pub fn handle_get_focus(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().focus_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!UpdateWindow`.
pub fn handle_update_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for UpdateWindow")?;

    let (window_proc, unicode) = {
        let windows = &state.window_state().windows;
        windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(window_handle))
            .map_or((0, false), |window| (window.window_proc, window.unicode))
    };

    if let Some(window) = find_window_mut(state, window_handle)
        && window.invalidated
    {
        window.invalidated = false;

        // UpdateWindow paints synchronously: erase the background first when
        // the invalidation requested it and a class brush exists (the
        // message-loop path synthesizes WM_ERASEBKGND ahead of WM_PAINT; here
        // the erase runs inline, DefWindowProc semantics).
        if window.flags.contains(WindowFlags::ERASE_BACKGROUND) {
            crate::user32::message::erase_window_background(state, window_handle);
        }

        // Synchronous WM_PAINT: the runtime bridges into the guest WndProc
        // and completes UpdateWindow with a fixed TRUE once it returns.
        if window_proc != 0 {
            return Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: window_proc,
                    window_handle,
                    message: WM_PAINT,
                    word_parameter: 0,
                    long_parameter: 0,
                    unicode,
                    outer_return: OuterReturn::Fixed(1),
                },
            }
            .into());
        }
    }

    // No update region or no guest WndProc: succeed without painting.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from UpdateWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!InvalidateRect`.
pub fn handle_invalidate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for InvalidateRect")?;

    let erase_background = engine
        .read_r8()
        .context("failed to read R8 for InvalidateRect")?;

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if let Some(window) = find_window_mut(state, window_handle) {
        window.invalidated = true;
        // bErase: OR so a TRUE erase request survives a later FALSE invalidation
        // of a different region (the update region accumulates, matching Windows).
        if erase_background != 0 {
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }

    // An explicit invalidation requests a visible change: bump the content
    // revision so the idle reconcile republishes the surface (an unknown
    // window resolves to nothing and is a silent no-op).
    crate::present::PresentState::request_paint(state, window_handle);

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from InvalidateRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!RedrawWindow`.
pub fn handle_redraw_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if let Some(window) = find_window_mut(state, window_handle) {
        window.invalidated = false;
    }

    // A requested redraw is a visible change: bump the content revision so
    // the idle reconcile republishes the surface (an unknown window resolves
    // to nothing and is a silent no-op).
    crate::present::PresentState::request_paint(state, window_handle);

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from RedrawWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!DestroyWindow`.
pub fn handle_destroy_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for DestroyWindow")?;

    // Extract window info before any mutation.
    let window_info =
        find_window(state, window_handle).map(|w| (w.window_proc, w.unicode, w.parent_handle));

    let Some((window_proc, unicode, parent_handle)) = window_info else {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from DestroyWindow")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    };

    // A destroyed top-level window publishes no frame, so the host window
    // registry would never wake to drop the stale winit window — request the
    // sync explicitly (the same wake the publish path fires). Children live
    // inside their parent's surface and have no host window of their own.
    if parent_handle == crate::handles::Hwnd::NULL {
        state.present().request_host_sync();
        // The top-level also leaves the host-visible window set and z-order.
        // The window-set revision bumps so the presenter's Frame handler
        // reconciles the stale host window away on the wake above — the
        // destroy side of the reconcile-on-change latch.
        state
            .present()
            .unregister_top_level(crate::handles::Hwnd::from(window_handle));
    }

    tracing::debug!(target: "wiegui", hwnd = window_handle, "DestroyWindow");

    if window_proc == 0 {
        // No WndProc — remove the window and return success.
        state
            .window_state()
            .windows
            .retain(|w| w.handle != crate::handles::Hwnd::from(window_handle));
        let ra = engine
            .return_from_win64_api(1)
            .context("failed to return from DestroyWindow")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 1,
        });
    }

    // Send WM_DESTROY to the window procedure.
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: window_proc,
            window_handle,
            message: WM_DESTROY,
            word_parameter: 0,
            long_parameter: 0,
            unicode,
            outer_return: OuterReturn::Fixed(1),
        },
    }
    .into())
}
