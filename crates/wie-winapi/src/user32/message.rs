use super::SC_CLOSE;
use super::{
    Context, FAKE_SYSTEM_COLOR_BRUSH_BASE, GuestCallbackRequest, HandlerContext,
    MessageQueueIdlePolicy, QueuedWindowMessage, Result, WM_CHAR, WM_CLOSE, WM_CONTEXTMENU,
    WM_DEADCHAR, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_MDICREATE, WM_PAINT, WM_QUIT,
    WM_SYSCHAR, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WS_CLIPCHILDREN,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WinMsg, WindowClassRecord,
    checked_field_address, create_mdi_child_from_struct, dispatch_control_proc, find_window,
    find_window_mut, is_known_window, read_guest_ansi_lossy, read_guest_u32, read_guest_u64,
    read_guest_utf16_lossy, register_window_class, write_message_structure,
};
use crate::OuterReturn;
use crate::gdi32::IRect;
use crate::gdi32::brush_color;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::subtract_rect;
use crate::user32::misc::timer_deadline;
use crate::user32::window::sys_color;

/// Whether `message` falls inside a `(min, max)` GetMessage/PeekMessage range.
///
/// A `(0, 0)` range means "no filter".
#[must_use]
fn message_range_matches(minimum: u32, maximum: u32, message: u32) -> bool {
    (minimum == 0 && maximum == 0) || (message >= minimum && message <= maximum)
}

/// Whether `window_handle` matches a GetMessage/PeekMessage window filter.
///
/// A filter of `NULL` matches every window. A dialog window's filter also
/// matches any of its descendants (the modal loop pulls messages for the
/// focused control inside the dialog).
#[must_use]
fn window_matches_filter(
    state: &WinApiState,
    window_filter: u64,
    window_handle: crate::handles::Hwnd,
) -> bool {
    window_filter == 0
        || window_handle == crate::handles::Hwnd::from(window_filter)
        || (is_dialog_window(state, window_filter)
            && descends_from_window(state, window_handle.as_u64(), window_filter))
}

/// Whether a queued message passes a GetMessage/PeekMessage filter.
///
/// `WM_QUIT` bypasses both the window filter and the message range entirely
/// (Microsoft Learn: `GetMessage` / `PeekMessage` do not filter `WM_QUIT`) —
/// a modal dialog's `GetMessage(dialog)` must still see the quit.
#[must_use]
fn message_matches_filter(
    state: &WinApiState,
    window_filter: u64,
    minimum_message: u32,
    maximum_message: u32,
    queued: &QueuedWindowMessage,
) -> bool {
    if queued.message == WM_QUIT {
        return true;
    }
    let window_matches = window_matches_filter(state, window_filter, queued.window_handle);
    let message_matches = (minimum_message == 0 && maximum_message == 0)
        || (queued.message >= minimum_message && queued.message <= maximum_message);
    window_matches && message_matches
}

/// Whether `window_handle` is a modal dialog window.
#[must_use]
fn is_dialog_window(state: &WinApiState, handle: u64) -> bool {
    state
        .try_window_state()
        .and_then(|ws| {
            ws.windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(handle))
        })
        .is_some_and(|w| w.dialog_proc != 0)
}

/// Whether `child` is `parent` or a descendant of `parent` (parent-chain walk).
#[must_use]
fn descends_from_window(state: &WinApiState, child: u64, parent: u64) -> bool {
    if child == parent {
        return true;
    }
    let mut current = child;
    loop {
        let Some(window) = state.try_window_state().and_then(|ws| {
            ws.windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(current))
        }) else {
            return false;
        };
        if window.parent_handle == crate::handles::Hwnd::from(parent) {
            return true;
        }
        if window.parent_handle == crate::handles::Hwnd::NULL
            || window.parent_handle == crate::handles::Hwnd::from(current)
        {
            return false;
        }
        current = window.parent_handle.as_u64();
    }
}

/// Decrement the queue's modal-dialog depth when a `WM_QUIT` is consumed.
///
/// `EndDialog` posts the quit; the modal loop consumes it. The depth gates
/// the `ExitOnIdle` synthetic-quit override (a dialog must never see the
/// regression `WM_QUIT`).
fn note_wm_quit_consumed(state: &mut WinApiState) {
    let mut queue = state.lock_message_queue();
    if queue.dialog_depth > 0 {
        queue.dialog_depth = queue.dialog_depth.saturating_sub(1);
        tracing::debug!(
            target: "wiegui",
            depth = queue.dialog_depth,
            "dialog depth down"
        );
    }
}

/// Handles of every window that matches a dialog window filter's descendant
/// rule (the filter window itself plus its whole subtree).
///
/// Precomputed with only immutable borrows so the message-synthesis paths can
/// query it while holding a mutable borrow of a disjoint `WindowState` field.
#[must_use]
fn dialog_filter_descendants(state: &WinApiState, window_filter: u64) -> Vec<crate::handles::Hwnd> {
    if !is_dialog_window(state, window_filter) {
        return Vec::new();
    }
    state.try_window_state().map_or_else(Vec::new, |ws| {
        ws.windows
            .iter()
            .filter(|window| descends_from_window(state, window.handle.as_u64(), window_filter))
            .map(|window| window.handle)
            .collect()
    })
}

/// Whether `message` is a keyboard-input message.
///
/// These are delivered to the focus window, not to whatever window they were
/// posted to, so the dequeue path rewrites their target first.
#[must_use]
fn is_keyboard_message(message: u32) -> bool {
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
            | WM_CONTEXTMENU
    )
}

/// Rewrite queued keyboard messages to the focus window before the filter
/// scan, so GetMessage/PeekMessage and DispatchMessage both see the effective
/// target (Windows sends keyboard input to the focused window regardless of
/// the window the key event was posted to).
///
/// Re-targeting only happens when a focus window is set AND it is a known
/// window; with no focus the queue is untouched (existing behavior).
fn retarget_keyboard_messages(state: &mut WinApiState) {
    let focus = state.window_state().focus_window_handle;
    if focus == crate::handles::Hwnd::NULL || !is_known_window(state, focus.as_u64()) {
        return;
    }
    let mut queue = state.lock_message_queue();
    for message in &mut queue.messages {
        if is_keyboard_message(message.message) && message.window_handle != focus {
            message.window_handle = focus;
        }
    }
}

/// Push `WM_TIMER` for every timer whose host-clock deadline has elapsed.
///
/// Timers are periodic: each fired timer's deadline is advanced to the next
/// interval even when the current message range/window filter excludes it, so
/// a narrow `GetMessage` cannot stall an unrelated timer forever.
///
/// Returns whether at least one `WM_TIMER` was queued.
fn synthesize_wm_timer(
    state: &mut WinApiState,
    window_filter: u64,
    minimum_message: u32,
    maximum_message: u32,
) -> Result<bool> {
    let now = std::time::Instant::now();
    let range_matches = message_range_matches(minimum_message, maximum_message, WM_TIMER);
    // Precompute the dialog-filter descendant set so the timer loop can keep
    // its mutable borrow of the timer list without aliasing `state`.
    let dialog_matches = dialog_filter_descendants(state, window_filter);
    let mut fired: Vec<(crate::handles::Hwnd, u64)> = Vec::new();
    {
        let timers = &mut state.window_state().timers;
        for timer in timers.iter_mut() {
            if timer.next_fire > now {
                continue;
            }
            let filter_matches = window_filter == 0
                || timer.window_handle == crate::handles::Hwnd::from(window_filter)
                || dialog_matches.contains(&timer.window_handle);
            if range_matches && filter_matches {
                fired.push((timer.window_handle, timer.timer_id));
            }
            timer.next_fire = timer_deadline(timer.interval_ms);
        }
    }
    if fired.is_empty() {
        return Ok(false);
    }

    let mut queue = state.lock_message_queue();
    for (window_handle, timer_id) in fired {
        tracing::debug!(
            target: "wiegui",
            timer_id,
            hwnd = window_handle.as_u64(),
            "WM_TIMER fired"
        );
        let time = queue.next_message_time;
        queue.next_message_time = queue
            .next_message_time
            .checked_add(1)
            .context("WM_TIMER synthesis timestamp overflow")?;
        queue.messages.push(QueuedWindowMessage {
            window_handle,
            message: WM_TIMER,
            word_parameter: timer_id,
            long_parameter: 0,
            time,
            point_x: 0,
            point_y: 0,
        });
    }
    Ok(true)
}

/// Push one `WM_PAINT` for the first invalidated window matching the filter.
///
/// The `invalidated` flag is cleared before the message is queued — Windows
/// generates `WM_PAINT` once per `InvalidateRect`/validate cycle, so a window
/// whose WndProc never calls `BeginPaint` cannot livelock the pump.
///
/// When the invalidation requested an erase (`InvalidateRect` `bErase`) AND
/// the window class has a background brush, a `WM_ERASEBKGND` is queued ahead
/// of the paint (Windows order). The erase message is dispatched host-side
/// (DefWindowProc semantics) and clears the pending-erase flag.
///
/// Returns whether a `WM_PAINT` was queued.
fn synthesize_wm_paint(
    state: &mut WinApiState,
    window_filter: u64,
    minimum_message: u32,
    maximum_message: u32,
) -> Result<bool> {
    if !message_range_matches(minimum_message, maximum_message, WM_PAINT) {
        return Ok(false);
    }

    let dialog_matches = dialog_filter_descendants(state, window_filter);
    let hwnd = {
        let windows = &state.window_state().windows;
        windows
            .iter()
            .find(|window| {
                window.invalidated
                    && (window_filter == 0
                        || window.handle == crate::handles::Hwnd::from(window_filter)
                        || dialog_matches.contains(&window.handle))
            })
            .map(|window| window.handle)
    };
    let Some(hwnd) = hwnd else {
        return Ok(false);
    };

    // Erase only when the invalidation asked for it AND a class brush exists
    // (DefWindowProc can only fill with a brush; without one the guest's
    // WM_PAINT sees fErase=1 and erases itself).
    let erase_background = {
        let windows = &state.window_state().windows;
        windows
            .iter()
            .find(|window| window.handle == hwnd)
            .is_some_and(|window| window.erase_background)
    } && class_brush_color(state, hwnd.as_u64()).is_some();

    if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
        window.invalidated = false;
    }

    let mut queue = state.lock_message_queue();
    let mut push = |message: u32| -> Result<()> {
        let time = queue.next_message_time;
        queue.next_message_time = queue
            .next_message_time
            .checked_add(1)
            .context("WM_PAINT synthesis timestamp overflow")?;
        queue.messages.push(QueuedWindowMessage {
            window_handle: hwnd,
            message,
            word_parameter: 0,
            long_parameter: 0,
            time,
            point_x: 0,
            point_y: 0,
        });
        Ok(())
    };
    if erase_background {
        push(WM_ERASEBKGND)?;
    }
    push(WM_PAINT)?;
    Ok(true)
}

/// Synthesize idle-priority messages (timers, then paint) into the queue.
///
/// Windows fires `WM_TIMER` and `WM_PAINT` only when no other message is
/// pending; the caller invokes this after an empty queue scan and re-scans
/// once afterwards. Timers are synthesized before paint (matching Windows'
/// queue priority between the two).
fn synthesize_idle_messages(
    state: &mut WinApiState,
    window_filter: u64,
    minimum_message: u32,
    maximum_message: u32,
) -> Result<bool> {
    if synthesize_wm_timer(state, window_filter, minimum_message, maximum_message)? {
        return Ok(true);
    }
    synthesize_wm_paint(state, window_filter, minimum_message, maximum_message)
}

/// The solid 0RGB color of a window class's background brush, if it has one.
///
/// Resolves the brush handle the way `WNDCLASS.hbrBackground` can carry it:
/// the classic `COLOR_x + 1` ordinal, a `GetSysColorBrush` handle
/// (`FAKE_SYSTEM_COLOR_BRUSH_BASE + index`), or a live `CreateSolidBrush`
/// record. Returns `None` for no brush / `NULL_BRUSH`.
fn class_brush_color(state: &mut WinApiState, hwnd: u64) -> Option<u32> {
    let brush = {
        let ws = state.try_window_state()?;
        let window = ws
            .windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))?;
        ws.window_classes
            .iter()
            .find(|class| {
                class.atom == window.class_atom
                    || (class.class_name == window.class_name && class.unicode == window.unicode)
            })
            .map_or(0, |class| class.background_brush)
    };
    if brush == 0 {
        return None;
    }
    // (HBRUSH)(COLOR_x + 1): the classic stock-brush convention.
    if brush <= 32 {
        return Some(sys_color(
            u32::try_from(brush.saturating_sub(1)).unwrap_or(0),
        ));
    }
    // GetSysColorBrush returns FAKE_SYSTEM_COLOR_BRUSH_BASE + color index.
    if let Some(index) = brush.checked_sub(FAKE_SYSTEM_COLOR_BRUSH_BASE)
        && index <= 32
    {
        return Some(sys_color(u32::try_from(index).unwrap_or(0)));
    }
    brush_color(state, crate::handles::Hbrush::from(brush))
}

/// Handle `WM_ERASEBKGND` host-side (DefWindowProc semantics): fill the
/// window's client area with its class background brush color.
///
/// The fill is clipped around visible children when the window has
/// `WS_CLIPCHILDREN` (the same `subtract_rect` decomposition the paint path
/// uses), so an erase can never cover a control. Clears the pending-erase
/// flag. Returns whether the background was erased (the WM_ERASEBKGND result).
pub(crate) fn erase_window_background(state: &mut WinApiState, hwnd: u64) -> bool {
    let Some(color) = class_brush_color(state, hwnd) else {
        // No class brush: nothing to fill (DefWindowProc returns 0).
        return false;
    };
    if let Some(window) = find_window_mut(state, hwnd) {
        window.erase_background = false;
    }
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return true;
    };
    let (width, height) =
        find_window(state, hwnd).map_or((0, 0), |window| (window.width, window.height));
    if width <= 0 || height <= 0 {
        return true;
    }
    let clip_children =
        find_window(state, hwnd).is_some_and(|window| window.style & WS_CLIPCHILDREN != 0);
    let mut rects = vec![IRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    }];
    if clip_children {
        let children: Vec<IRect> = state
            .window_state()
            .windows
            .iter()
            .filter(|window| {
                window.parent_handle == crate::handles::Hwnd::from(hwnd) && window.visible
            })
            .map(|window| IRect {
                left: window.x,
                top: window.y,
                right: window.x.saturating_add(window.width),
                bottom: window.y.saturating_add(window.height),
            })
            .collect();
        for child in children {
            rects = subtract_rect(rects, child);
        }
    }
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left.saturating_add(info.offset_x),
            rect.top.saturating_add(info.offset_y),
            rect.width(),
            rect.height(),
            color,
        );
    }
    true
}

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

/// Handles `USER32.dll!RegisterClassA`.
pub fn handle_register_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassA")?;

    if class_ptr == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from RegisterClassA")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // WNDCLASSA on Win64:
    //  +0x00 style (u32)
    //  +0x08 lpfnWndProc (u64)
    //  +0x10 cbClsExtra (i32)
    //  +0x14 cbWndExtra (i32)
    //  +0x18 hInstance (u64)
    //  +0x20 hIcon (u64)
    //  +0x28 hCursor (u64)
    //  +0x30 hbrBackground (u64)
    //  +0x38 lpszMenuName (u64)
    //  +0x40 lpszClassName (u64)

    let style = read_guest_u32(engine, class_ptr)?;
    let window_proc = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 8, "WNDCLASS.lpfnWndProc"),
    )?;
    let _cls_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x10, "WNDCLASS.cbClsExtra"),
    )?;
    let _wnd_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x14, "WNDCLASS.cbWndExtra"),
    )?;
    let instance_handle = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x18, "WNDCLASS.hInstance"),
    )?;
    let icon = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x20, "WNDCLASS.hIcon"),
    )?;
    let cursor = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x28, "WNDCLASS.hCursor"),
    )?;
    let background_brush = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x30, "WNDCLASS.hbrBackground"),
    )?;
    let _menu_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x38, "WNDCLASS.lpszMenuName"),
    )?;
    let class_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x40, "WNDCLASS.lpszClassName"),
    )?;

    let class_name = if class_name_ptr == 0 {
        String::new()
    } else {
        read_guest_ansi_lossy(engine, class_name_ptr, 256)
            .context("failed to read WNDCLASS.lpszClassName for RegisterClassA")?
    };

    let atom = register_window_class(
        state,
        WindowClassRecord {
            atom: 0,
            class_name,
            window_proc,
            style,
            instance_handle,
            icon_handle: icon,
            cursor_handle: cursor,
            background_brush,
            small_icon_handle: 0,
            unicode: false,
        },
    )
    .context("failed to register window class for RegisterClassA")?;

    let ra = engine
        .return_from_win64_api(atom)
        .context("failed to return from RegisterClassA")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: atom,
    })
}

/// Handles `USER32.dll!RegisterClassW`.
pub fn handle_register_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassW")?;

    if class_ptr == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from RegisterClassW")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // WNDCLASSW on Win64 (same layout as WNDCLASSA, but lpszClassName
    // and lpszMenuName point to UTF-16 strings):
    //  +0x00 style (u32)
    //  +0x08 lpfnWndProc (u64)
    //  +0x10 cbClsExtra (i32)
    //  +0x14 cbWndExtra (i32)
    //  +0x18 hInstance (u64)
    //  +0x20 hIcon (u64)
    //  +0x28 hCursor (u64)
    //  +0x30 hbrBackground (u64)
    //  +0x38 lpszMenuName (u64)
    //  +0x40 lpszClassName (u64)

    let style = read_guest_u32(engine, class_ptr)?;
    let window_proc = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 8, "WNDCLASSW.lpfnWndProc"),
    )?;
    let _cls_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x10, "WNDCLASSW.cbClsExtra"),
    )?;
    let _wnd_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x14, "WNDCLASSW.cbWndExtra"),
    )?;
    let instance_handle = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x18, "WNDCLASSW.hInstance"),
    )?;
    let icon = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x20, "WNDCLASSW.hIcon"),
    )?;
    let cursor = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x28, "WNDCLASSW.hCursor"),
    )?;
    let background_brush = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x30, "WNDCLASSW.hbrBackground"),
    )?;
    let _menu_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x38, "WNDCLASSW.lpszMenuName"),
    )?;
    let class_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x40, "WNDCLASSW.lpszClassName"),
    )?;

    let class_name = if class_name_ptr == 0 {
        String::new()
    } else {
        read_guest_utf16_lossy(engine, class_name_ptr, 256)
            .context("failed to read WNDCLASSW.lpszClassName for RegisterClassW")?
    };

    let atom = register_window_class(
        state,
        WindowClassRecord {
            atom: 0,
            class_name,
            window_proc,
            style,
            instance_handle,
            icon_handle: icon,
            cursor_handle: cursor,
            background_brush,
            small_icon_handle: 0,
            unicode: true,
        },
    )
    .context("failed to register window class for RegisterClassW")?;

    let ra = engine
        .return_from_win64_api(atom)
        .context("failed to return from RegisterClassW")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: atom,
    })
}

/// Handles `USER32.dll!UnregisterClassA`.
pub fn handle_unregister_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from UnregisterClassA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!UnregisterClassW`.
pub fn handle_unregister_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from UnregisterClassW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!ValidateRect`.
pub fn handle_validate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ValidateRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!SetWindowLongA`.
pub fn handle_set_window_long_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetWindowLongA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `USER32.dll!SetWindowLongW`.
pub fn handle_set_window_long_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetWindowLongW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `USER32.dll!GetWindowDC`.
pub fn handle_get_window_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dc = super::FAKE_DEVICE_CONTEXT_HANDLE;
    let return_address = engine
        .return_from_win64_api(dc)
        .context("failed to return from GetWindowDC")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: dc,
    })
}
