//! The modal dialog lifecycle.
//!
//! `CreateDialogParamA/W` resolves the `hInstance`+id template, builds the
//! dialog window (parented to the owner so it composites into the owner's
//! present surface) plus one child `WindowRecord` per template item, sends
//! `WM_INITDIALOG` to the guest dialog proc through the callback bridge, and
//! bumps the queue's dialog depth.
//!
//! `EndDialog` writes the result into the fixed guest dialog-result slot,
//! posts `WM_QUIT` (the stub's loop exits on it), removes the dialog subtree
//! and invalidates the owner so its next repaint erases the region.

use anyhow::{Context, Result};

use crate::OuterReturn;
use crate::state::WindowFlags;
use crate::user32::{
    BS_DEFPUSHBUTTON, CreateWindowRequest, GuestCallbackRequest, HandlerContext,
    QueuedWindowMessage, WM_INITDIALOG, WM_QUIT, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WindowClassIdentifier,
    controls::ControlClassKind, create_window_record, find_window, find_window_mut, read_u64,
    window_client_size,
};
use wie_pe::resources::{DialogItemTemplate, DialogTemplate, ItemClass};

use super::activate_modal_dialog;
use super::template::resolve_template;

/// Handles `USER32.dll!CreateDialogParamA`.
pub fn handle_create_dialog_param_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_dialog_param(ctx, false, "CreateDialogParamA")
}
/// Handles `USER32.dll!CreateDialogParamW`.
pub fn handle_create_dialog_param_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_dialog_param(ctx, true, "CreateDialogParamW")
}

/// Shared `CreateDialogParamA/W` implementation.
fn handle_create_dialog_param(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let template_value = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let parent_handle = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let dialog_proc = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let init_param = read_u64(engine, rsp.wrapping_add(0x28))
        .with_context(|| format!("failed to read 5th arg for {api_name}"))?;

    let template = resolve_template(
        state,
        ctx.environment.image_base,
        instance_handle,
        template_value,
    );

    let (dialog_hwnd, subtree) = build_dialog_window(
        state,
        &template,
        parent_handle,
        instance_handle,
        dialog_proc,
        unicode,
    )?;

    if dialog_hwnd == 0 {
        return ctx.finish(0);
    }

    tracing::info!(
        target: "wiegui",
        template_id = template.name,
        hwnd = dialog_hwnd,
        parent = parent_handle,
        "dialog opened"
    );

    // A fresh dialog must not observe a stale result from a previous one.
    let result_va = state.window_state().dialog_result_va;
    if result_va != 0 {
        drop(engine.mem_write(result_va, &0_u32.to_le_bytes()));
    }

    // The dialog is modal (an empty GetMessage must yield, not synthesize the
    // regression-mode WM_QUIT), takes activation, and its first WS_TABSTOP
    // child in creation order gets the initial keyboard focus — Windows gives
    // it focus and passes it as WM_INITDIALOG's wParam, so the first
    // keystrokes land on the dialog instead of the owner and GetFocus() reads
    // a control.
    let first_tabstop = first_tabstop_child(state, dialog_hwnd);
    let _unused = activate_modal_dialog(
        state,
        engine,
        dialog_hwnd,
        (first_tabstop != 0).then_some(first_tabstop),
        &subtree,
    )?;

    if dialog_proc == 0 {
        // No dialog proc: nothing to bridge, the dialog just exists.
        return ctx.finish(dialog_hwnd);
    }

    // WM_INITDIALOG through the established callback bridge. `Fixed(hwnd)`
    // (not `CreateWindow(hwnd)`) so a dialog proc returning -1 cannot abort
    // creation — only WM_CREATE's -1 is an abort signal on Windows.
    tracing::debug!(
        target: "wiegui",
        hwnd = dialog_hwnd,
        "WM_INITDIALOG delivered"
    );
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: dialog_proc,
            window_handle: dialog_hwnd,
            message: WM_INITDIALOG,
            word_parameter: first_tabstop,
            long_parameter: init_param,
            unicode,
            outer_return: OuterReturn::Fixed(dialog_hwnd),
        },
    }
    .into())
}

/// Create the dialog window record plus one child per template item.
///
/// The dialog is parented to the owner so it composites into the owner's
/// present surface via the existing `resolve_window_ancestor` offset walk.
/// Returns the dialog handle and the whole subtree (dialog + children).
fn build_dialog_window(
    state: &mut WinApiState,
    template: &DialogTemplate,
    parent_handle: u64,
    instance_handle: u64,
    dialog_proc: u64,
    unicode: bool,
) -> Result<(u64, Vec<u64>)> {
    let (cx, cy) = (template.pixel_rect.cx, template.pixel_rect.cy);
    let (x, y) = center_in_owner(state, parent_handle, template, cx, cy);

    let style = template.style | WS_VISIBLE | WS_CLIPCHILDREN;
    let (hwnd, _window_proc, _class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("Dialog".to_owned()),
            title: template.title.clone(),
            style,
            extended_style: template.ex_style,
            parent_handle,
            menu_handle: 0,
            instance_handle,
            x,
            y,
            width: cx,
            height: cy,
        },
        unicode,
    )?;
    if hwnd == 0 {
        return Ok((0, Vec::new()));
    }

    let mut subtree = vec![hwnd];
    if let Some(window) = find_window_mut(state, hwnd) {
        window.dialog_proc = dialog_proc;
        window.dialog_unicode = unicode;
        window.client_rect = (0, 0, cx, cy);
    }

    for item in &template.items {
        if let Some(child) = build_dialog_item(state, hwnd, instance_handle, item, unicode)? {
            subtree.push(child);
        }
    }
    Ok((hwnd, subtree))
}

/// Create one dialog control (a built-in-class child of the dialog window).
fn build_dialog_item(
    state: &mut WinApiState,
    dialog_hwnd: u64,
    instance_handle: u64,
    item: &DialogItemTemplate,
    unicode: bool,
) -> Result<Option<u64>> {
    let Some(class_identifier) = item_class_identifier(&item.class) else {
        // Unknown control classes have no host-side WndProc; skip them.
        return Ok(None);
    };
    let mut style = item.style | WS_CHILD | WS_VISIBLE;
    if matches!(item.class, ItemClass::Button) {
        style |= WS_TABSTOP;
    }
    let (hwnd, _window_proc, _class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title: item.title.clone(),
            style,
            extended_style: item.ex_style,
            parent_handle: dialog_hwnd,
            menu_handle: u64::from(item.id),
            instance_handle,
            x: item.pixel_rect.x,
            y: item.pixel_rect.y,
            width: item.pixel_rect.cx,
            height: item.pixel_rect.cy,
        },
        unicode,
    )?;
    if hwnd != 0
        && let Some(window) = find_window_mut(state, hwnd)
    {
        window.client_rect = (0, 0, item.pixel_rect.cx, item.pixel_rect.cy);
        // BS_DEFPUSHBUTTON marks the dialog's Enter default button (the
        // control state is created lazily; record it here for IsDialogMessage).
        if matches!(item.class, ItemClass::Button) && style & BS_DEFPUSHBUTTON != 0 {
            state
                .window_state()
                .control_states
                .entry(crate::handles::Hwnd::from(hwnd))
                .or_insert_with(|| ControlClassKind::Button.new_state())
                .set_default_push(true);
        }
    }
    Ok((hwnd != 0).then_some(hwnd))
}

/// Map a parsed template item class to a built-in control class identifier.
fn item_class_identifier(class: &ItemClass) -> Option<WindowClassIdentifier> {
    let ordinal = match class {
        ItemClass::Button => 0x0080,
        ItemClass::Edit => 0x0081,
        ItemClass::Static => 0x0082,
        ItemClass::ListBox => 0x0083,
        ItemClass::ComboBox => 0x0085,
        ItemClass::Other(_) => return None,
    };
    Some(WindowClassIdentifier::Atom(ordinal))
}

/// The first visible `WS_TABSTOP` child of a dialog, in creation order.
///
/// Windows gives this control default keyboard focus when the dialog opens and
/// passes its handle as `WM_INITDIALOG`'s `wParam`.
#[must_use]
fn first_tabstop_child(state: &WinApiState, dialog_hwnd: u64) -> u64 {
    state.try_window_state().map_or(0, |ws| {
        ws.windows
            .iter()
            .find(|w| {
                w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                    && w.visible
                    && w.style & WS_TABSTOP != 0
            })
            .map_or(0, |w| w.handle.as_u64())
    })
}

/// Center the dialog over its owner; fall back to the template origin.
fn center_in_owner(
    state: &mut WinApiState,
    parent_handle: u64,
    template: &DialogTemplate,
    cx: i32,
    cy: i32,
) -> (i32, i32) {
    let (owner_w, owner_h) = if parent_handle != 0 {
        window_client_size(state, parent_handle)
    } else {
        (0, 0)
    };
    if owner_w >= cx && owner_h >= cy {
        (
            owner_w.saturating_sub(cx).saturating_div(2),
            owner_h.saturating_sub(cy).saturating_div(2),
        )
    } else {
        (template.pixel_rect.x, template.pixel_rect.y)
    }
}

/// Handles `USER32.dll!EndDialog`.
pub fn handle_end_dialog(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .context("failed to read RCX for EndDialog")?;
    let result = engine
        .read_rdx()
        .context("failed to read RDX for EndDialog")?;

    let is_dialog = find_window(state, dialog_hwnd).is_some_and(|w| w.dialog_proc != 0);
    let return_value = if is_dialog {
        // A closing interactive file dialog writes its chosen path back into
        // the guest's OPENFILENAME buffer before the modal loop returns
        // (GetOpenFileNameW's TRUE/FALSE is the EndDialog result). A closing
        // font dialog writes the selection into the guest's LOGFONTW /
        // CHOOSEFONTW instead — or, for an effects-toggle sentinel result,
        // toggles the checkbox and keeps the dialog open (`None`). The
        // effective result (0 when canceled) is what the loop returns to the
        // guest.
        let result = if crate::comdlg32::is_font_dialog_window(state, dialog_hwnd) {
            let Some(font_result) =
                crate::comdlg32::complete_font_dialog(engine, state, dialog_hwnd, result)
                    .context("EndDialog: font-dialog write-back failed")?
            else {
                // Effects toggle: the dialog stays open — no result slot
                // write, no WM_QUIT, no teardown. The stub's EndDialog call
                // returns and the modal loop keeps pumping.
                tracing::debug!(
                    target: "wiegui",
                    hwnd = dialog_hwnd,
                    "EndDialog: font effects toggle (dialog stays open)"
                );
                return ctx.finish(1);
            };
            font_result
        } else {
            crate::comdlg32::complete_file_dialog(engine, state, dialog_hwnd, result)
                .context("EndDialog: file-dialog write-back failed")?
        };
        let result = u64::from(u32::try_from(result & u64::from(u32::MAX)).unwrap_or(0));
        tracing::info!(
            target: "wiegui",
            hwnd = dialog_hwnd,
            result,
            "EndDialog"
        );
        // Write the result where the in-guest stub reads it after WM_QUIT.
        let result_va = state.window_state().dialog_result_va;
        if result_va != 0 {
            let low = u32::try_from(result & u64::from(u32::MAX)).unwrap_or(0);
            drop(engine.mem_write(result_va, &low.to_le_bytes()));
        }

        // Post WM_QUIT — the modal stub's GetMessage loop exits on it. The
        // dialog depth is decremented when that WM_QUIT is consumed.
        {
            let mut queue = state.lock_message_queue();
            let time = queue.next_message_time;
            queue.next_message_time = time.wrapping_add(1);
            queue.messages.push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::NULL,
                message: WM_QUIT,
                word_parameter: result,
                long_parameter: 0,
                time,
                point_x: 0,
                point_y: 0,
            });
        }

        // Remove the dialog subtree, then rerender the WHOLE window in one
        // cycle: invalidate the owner (with erase, so the class brush
        // repaints its background over the dialog region) and every remaining
        // descendant (so controls repaint over the dialog face too — no
        // piecemeal per-control disappearance, no patchy background).
        let owner = remove_dialog_subtree(state, dialog_hwnd);
        if let Some(window) = find_window_mut(state, owner) {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
        invalidate_subtree(state, crate::handles::Hwnd::from(owner));

        1
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Remove `dialog_hwnd` and every descendant window from the runtime state.
///
/// Returns the dialog's owner handle (0 when unknown) so the caller can
/// invalidate it.
fn remove_dialog_subtree(state: &mut WinApiState, dialog_hwnd: u64) -> u64 {
    let owner = find_window(state, dialog_hwnd).map_or(0, |w| w.parent_handle.as_u64());
    let mut doomed: Vec<crate::handles::Hwnd> = vec![crate::handles::Hwnd::from(dialog_hwnd)];
    loop {
        let before = doomed.len();
        for window in &state.window_state().windows {
            if doomed.contains(&window.parent_handle) && !doomed.contains(&window.handle) {
                doomed.push(window.handle);
            }
        }
        if doomed.len() == before {
            break;
        }
    }
    state
        .window_state()
        .windows
        .retain(|w| !doomed.contains(&w.handle));
    for hwnd in doomed {
        state.window_state().control_states.remove(&hwnd);
    }
    owner
}

/// Mark `hwnd` and every descendant invalidated so the next paint cycle
/// repaints the whole subtree in one pass — the full-window rerender after a
/// modal dialog closes (the dialog composited into the owner surface, so the
/// owner background and all remaining controls must repaint over its region).
fn invalidate_subtree(state: &mut WinApiState, hwnd: crate::handles::Hwnd) {
    if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
        window.invalidated = true;
    }
    let mut frontier: Vec<crate::handles::Hwnd> = vec![hwnd];
    while !frontier.is_empty() {
        let mut next: Vec<crate::handles::Hwnd> = Vec::new();
        for window in &mut state.window_state().windows {
            if frontier.contains(&window.parent_handle) {
                window.invalidated = true;
                next.push(window.handle);
            }
        }
        frontier = next;
    }
}
