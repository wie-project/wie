//! The dialog-proc surface: `IsDialogMessageA/W` keyboard translation and
//! `DefDlgProcA/W`.
//!
//! `IsDialogMessageA/W` handles Tab / Shift+Tab focus movement across
//! `WS_TABSTOP` children and Enter / Esc → `WM_COMMAND(IDOK/IDCANCEL)` to
//! the dialog proc.

use anyhow::{Context, Result};

use crate::OuterReturn;
use crate::user32::{
    BN_CLICKED, GuestCallbackRequest, HandlerContext, IDCANCEL, VK_ESCAPE, VK_RETURN, VK_SHIFT,
    VK_TAB, WM_COMMAND, WM_KEYDOWN, WS_TABSTOP, WinApiControlSignal, WinApiHandlerResult,
    WinApiState, checked_address, controls::ControlClassKind, controls::ControlState, find_window,
    make_command_wparam, message::handle_default_window_procedure, read_u32, read_u64,
    window::deliver_focus_change,
};

/// Outcome of an `IsDialogMessage` keyboard translation.
enum DialogKeyAction {
    /// Not a dialog-navigation key — caller continues normal dispatch.
    None,
    /// Tab / Shift+Tab — host-side focus movement only.
    Focus,
    /// Enter — activate a push button (default or focused): bridge
    /// `WM_COMMAND(BN_CLICKED)` with the button's id to the dialog proc.
    ActivateButton(u64),
    /// Esc — bridge `WM_COMMAND(IDCANCEL)` to the dialog proc.
    Command(u64),
}

/// Handles `USER32.dll!IsDialogMessageA`.
pub fn handle_is_dialog_message_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_is_dialog_message(ctx, "IsDialogMessageA")
}
/// Handles `USER32.dll!IsDialogMessageW`.
pub fn handle_is_dialog_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_is_dialog_message(ctx, "IsDialogMessageW")
}

/// Shared `IsDialogMessageA/W` implementation.
fn handle_is_dialog_message(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let message_address = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let (dialog_proc, dialog_unicode) = find_window(state, dialog_hwnd)
        .filter(|w| w.dialog_proc != 0)
        .map_or((0, false), |w| (w.dialog_proc, w.dialog_unicode));

    // Host-owned modeless dialogs (comdlg32 Find/Replace) have no guest
    // dialog proc, but their buttons still answer Enter/Escape — the command
    // is handled host-side instead of bridged.
    let host_find_dialog = crate::comdlg32::is_find_dialog_window(state, dialog_hwnd);

    if (dialog_proc == 0 && !host_find_dialog) || message_address == 0 {
        // Not a dialog (or no message): caller continues normal dispatch.
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let message = read_u32(engine, checked_address(message_address, 8, "MSG.message"))
        .with_context(|| format!("failed to read MSG.message for {api_name}"))?;
    let word_parameter = read_u64(engine, checked_address(message_address, 16, "MSG.wParam"))
        .with_context(|| format!("failed to read MSG.wParam for {api_name}"))?;

    // Tab navigation is fully host-side; Enter/Esc become WM_COMMAND to the
    // dialog proc (bridged, so EndDialog inside the proc works). Enter
    // activates the default push button (BS_DEFPUSHBUTTON); with no default
    // it activates the focused push button; with neither it is not consumed.
    let action = match message {
        WM_KEYDOWN => match word_parameter & 0xFF {
            VK_TAB => DialogKeyAction::Focus,
            VK_RETURN => {
                let target = default_push_button(state, dialog_hwnd)
                    .or_else(|| focused_dialog_button(state, dialog_hwnd));
                target.map_or(DialogKeyAction::None, DialogKeyAction::ActivateButton)
            }
            VK_ESCAPE => DialogKeyAction::Command(IDCANCEL),
            _ => DialogKeyAction::None,
        },
        _ => DialogKeyAction::None,
    };

    match action {
        DialogKeyAction::None => {
            let return_address = engine
                .return_from_win64_api(0)
                .with_context(|| format!("failed to return from {api_name}"))?;
            Ok(WinApiHandlerResult {
                return_address,
                return_value: 0,
            })
        }
        DialogKeyAction::Focus => {
            tracing::debug!(
                target: "wiegui",
                hwnd = dialog_hwnd,
                "IsDialogMessage: Tab focus move"
            );
            if let Some(signal) =
                advance_dialog_focus(state, engine, dialog_hwnd, vk_is_shift_down(state))?
            {
                return Err(signal.into());
            }
            let return_address = engine
                .return_from_win64_api(1)
                .with_context(|| format!("failed to return from {api_name}"))?;
            Ok(WinApiHandlerResult {
                return_address,
                return_value: 1,
            })
        }
        DialogKeyAction::ActivateButton(button_hwnd) => {
            tracing::debug!(
                target: "wiegui",
                hwnd = dialog_hwnd,
                button = button_hwnd,
                "IsDialogMessage: Enter activates button"
            );
            // The button's control id (stored in its menu_handle) is the
            // WM_COMMAND id the dialog proc sees — identical to a click.
            let id = find_window(state, button_hwnd).map_or(0, |w| w.menu_handle) & 0xFFFF;
            if host_find_dialog {
                // A host find-dialog button commands the host dialog (which
                // posts FINDMSGSTRING), not the guest; the message is
                // consumed and IsDialogMessage completes with TRUE.
                crate::comdlg32::handle_find_dialog_command(
                    engine,
                    state,
                    dialog_hwnd,
                    make_command_wparam(id, BN_CLICKED),
                    button_hwnd,
                )
                .context("host find dialog Enter command failed")?;
                let return_address = engine
                    .return_from_win64_api(1)
                    .with_context(|| format!("failed to return from {api_name}"))?;
                return Ok(WinApiHandlerResult {
                    return_address,
                    return_value: 1,
                });
            }
            // Consumed: the caller's DispatchMessage must not see the message.
            // The guest dialog proc runs synchronously (WM_COMMAND may call
            // EndDialog), then IsDialogMessage completes with TRUE.
            Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: dialog_proc,
                    window_handle: dialog_hwnd,
                    message: WM_COMMAND,
                    word_parameter: make_command_wparam(id, BN_CLICKED),
                    long_parameter: 0,
                    unicode: dialog_unicode,
                    outer_return: OuterReturn::Fixed(1),
                },
            }
            .into())
        }
        DialogKeyAction::Command(id) => {
            tracing::debug!(
                target: "wiegui",
                hwnd = dialog_hwnd,
                id,
                "IsDialogMessage: Enter/Esc command"
            );
            if host_find_dialog {
                // Escape on a host find dialog is its Cancel button: the host
                // dialog posts FR_DIALOGTERM and tears itself down.
                crate::comdlg32::handle_find_dialog_command(
                    engine,
                    state,
                    dialog_hwnd,
                    make_command_wparam(id, BN_CLICKED),
                    0,
                )
                .context("host find dialog Escape command failed")?;
                let return_address = engine
                    .return_from_win64_api(1)
                    .with_context(|| format!("failed to return from {api_name}"))?;
                return Ok(WinApiHandlerResult {
                    return_address,
                    return_value: 1,
                });
            }
            // Consumed: the caller's DispatchMessage must not see the message.
            // The guest dialog proc runs synchronously (WM_COMMAND may call
            // EndDialog), then IsDialogMessage completes with TRUE.
            Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: dialog_proc,
                    window_handle: dialog_hwnd,
                    message: WM_COMMAND,
                    word_parameter: make_command_wparam(id, BN_CLICKED),
                    long_parameter: 0,
                    unicode: dialog_unicode,
                    outer_return: OuterReturn::Fixed(1),
                },
            }
            .into())
        }
    }
}

/// Advance keyboard focus across a dialog's `WS_TABSTOP` children.
///
/// `reverse` mirrors Shift+Tab. Focus wraps at both ends, matching Windows.
/// The move delivers `WM_KILLFOCUS(old)` + `WM_SETFOCUS(new)` via
/// [`deliver_focus_change`]; a guest-WndProc target returns a bridge signal
/// for the caller to propagate.
fn advance_dialog_focus(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    dialog_hwnd: u64,
    reverse: bool,
) -> Result<Option<WinApiControlSignal>> {
    let tabstop: Vec<crate::handles::Hwnd> = state
        .window_state()
        .windows
        .iter()
        .filter(|w| {
            w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                && w.visible
                && w.style & WS_TABSTOP != 0
        })
        .map(|w| w.handle)
        .collect();
    if tabstop.is_empty() {
        return Ok(None);
    }
    let len = tabstop.len();
    let current = state.window_state().focus_window_handle;
    let current_index = tabstop.iter().position(|&h| h == current);
    let next_index = match (current_index, reverse) {
        (Some(index), false) => {
            if index.saturating_add(1) >= len {
                0
            } else {
                index.saturating_add(1)
            }
        }
        (Some(index), true) => {
            if index == 0 {
                len.saturating_sub(1)
            } else {
                index.saturating_sub(1)
            }
        }
        (None, false) => 0,
        (None, true) => len.saturating_sub(1),
    };
    let next = tabstop
        .get(next_index)
        .copied()
        .unwrap_or(crate::handles::Hwnd::NULL);
    state.window_state().focus_window_handle = next;
    deliver_focus_change(
        state,
        engine,
        current.as_u64(),
        next.as_u64(),
        OuterReturn::Fixed(1),
    )
}

/// The dialog's default push button (`BS_DEFPUSHBUTTON`), in creation order.
///
/// Windows resolves Enter against this button regardless of the focused
/// control.
#[must_use]
fn default_push_button(state: &WinApiState, dialog_hwnd: u64) -> Option<u64> {
    let ws = state.try_window_state()?;
    let default: Vec<crate::handles::Hwnd> = ws
        .windows
        .iter()
        .filter(|w| {
            w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                && w.visible
                && w.control_kind == Some(ControlClassKind::Button)
        })
        .filter(|w| {
            ws.control_states
                .get(&w.handle)
                .is_some_and(ControlState::is_default_push)
        })
        .map(|w| w.handle)
        .collect();
    default.first().copied().map(crate::handles::Hwnd::as_u64)
}

/// The focused push button of a dialog, when the focus is one of its buttons.
///
/// With no default button, Enter activates the focused button (BM_CLICK-style
/// activation).
#[must_use]
fn focused_dialog_button(state: &WinApiState, dialog_hwnd: u64) -> Option<u64> {
    let focus = state.try_window_state()?.focus_window_handle;
    state
        .try_window_state()?
        .windows
        .iter()
        .find(|w| {
            w.handle == focus
                && w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                && w.control_kind == Some(ControlClassKind::Button)
        })
        .map(|w| w.handle.as_u64())
}

/// Whether the Shift key is held (per the guest keyboard state).
fn vk_is_shift_down(state: &WinApiState) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.keyboard_state
            .get(usize::try_from(VK_SHIFT).unwrap_or(0))
            & 0x80
            != 0
    })
}

/// Handles `USER32.dll!DefDlgProcA` (delegates to the default window proc).
pub fn handle_def_dlg_proc_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefDlgProcA")
}
/// Handles `USER32.dll!DefDlgProcW`.
pub fn handle_def_dlg_proc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_default_window_procedure(ctx, "DefDlgProcW")
}
