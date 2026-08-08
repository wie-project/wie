//! Message-filter helpers and idle-message synthesis (timers, paint, erase)
//! plus the class-brush background machinery (split from `message.rs`).

use crate::gdi32::IRect;
use crate::gdi32::brush_color;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::subtract_rect;
use crate::state::WindowFlags;
use crate::user32::misc::timer_deadline;
use crate::user32::window::sys_color;
use crate::user32::{
    FAKE_SYSTEM_COLOR_BRUSH_BASE, QueuedWindowMessage, Result, WM_CHAR, WM_CONTEXTMENU,
    WM_DEADCHAR, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_PAINT, WM_QUIT, WM_SYSCHAR,
    WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WS_CLIPCHILDREN, WinApiState,
    find_window, find_window_mut, is_known_window,
};

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
pub(super) fn message_matches_filter(
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
pub(super) fn retarget_keyboard_messages(state: &mut WinApiState) {
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
        queue.push(window_handle, WM_TIMER, timer_id, 0)?;
    }
    Ok(true)
}

/// Push one `WM_PAINT` for the first invalidated window matching the filter.
///
/// The `invalidated` flag is cleared before the message is queued — Windows
/// generates `WM_PAINT` once per `InvalidateRect`/validate cycle, so a window
/// whose WndProc never calls `BeginPaint` cannot livelock the pump.
///
/// A HIDDEN window (clear `WS_VISIBLE`) is never selected: real Windows
/// discards a hidden window's invalidated region and never sends it a
/// WM_PAINT. Without this, an invalidation that predates a `ShowWindow(
/// SW_HIDE)` (the status bar's `SB_SETTEXTW` outliving the hide) would be
/// synthesized and dispatched after the hide, re-painting the hidden bar's
/// strip over the control that grew into its space (the live View > Status
/// Bar regression). The dispatch arm keeps its own visibility gate as
/// defense-in-depth; the synthesis gate is the Windows-faithful first check.
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
                    && window.visible
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
            .is_some_and(|window| window.flags.contains(WindowFlags::ERASE_BACKGROUND))
    } && class_brush_color(state, hwnd.as_u64()).is_some();

    if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
        window.invalidated = false;
    }

    let mut queue = state.lock_message_queue();
    if erase_background {
        queue.push(hwnd, WM_ERASEBKGND, 0, 0)?;
    }
    queue.push(hwnd, WM_PAINT, 0, 0)?;
    Ok(true)
}

/// Synthesize idle-priority messages (timers, then paint) into the queue.
///
/// Windows fires `WM_TIMER` and `WM_PAINT` only when no other message is
/// pending; the caller invokes this after an empty queue scan and re-scans
/// once afterwards. Timers are synthesized before paint (matching Windows'
/// queue priority between the two).
pub(super) fn synthesize_idle_messages(
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
        window.flags.remove(WindowFlags::ERASE_BACKGROUND);
    }
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return true;
    };
    // F2 no-black: record the erase color as the owning surface's background
    // so the presenter clears its surface with it — regions the frame does
    // not cover (resize seams, pre-first-paint) never read black.
    state.present().set_background_color(info.hwnd, color);
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
            .map(|window| IRect::from_xywh(window.x, window.y, window.width, window.height))
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
    // The erase painted over the surface this window owns, INCLUDING any
    // controls beneath it (a window without `WS_CLIPCHILDREN` erases over
    // its children — real Windows repaints them afterward). The EDIT's
    // row-band optimization assumes its surface base is intact; a full
    // erase destroys that base, so a band-limited repaint would leave the
    // erased rows blank (the Go To line-N blank-rows bug: the modal dialog
    // close erases the owner white, then the EDIT repaints only its pending
    // band). Reset every EDIT in the subtree to a full repaint.
    crate::user32::controls::reset_edit_bands_in_subtree(state, hwnd);
    true
}
