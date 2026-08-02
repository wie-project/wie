//! Modal dialog machinery: `CreateDialogParamA/W`, `EndDialog`,
//! `IsDialogMessageA/W`, `GetDlgItem*`, `SetDlgItemText*`, `DefDlgProc*`.
//!
//! The modal loop itself runs in-guest as a `GuestStubKind::DialogBoxParam`
//! stub (see `wie-runtime/src/guest_stubs.rs`); every piece of dialog logic
//! lives here on the host:
//!
//! * `CreateDialogParamA/W` resolves the `hInstance`+id template, builds the
//!   dialog window (parented to the owner so it composites into the owner's
//!   present surface) plus one child `WindowRecord` per template item, sends
//!   `WM_INITDIALOG` to the guest dialog proc through the callback bridge,
//!   and bumps the queue's dialog depth.
//! * `EndDialog` writes the result into the fixed guest dialog-result slot,
//!   posts `WM_QUIT` (the stub's loop exits on it), removes the dialog
//!   subtree and invalidates the owner so its next repaint erases the region.
//! * `IsDialogMessageA/W` handles Tab / Shift+Tab focus movement across
//!   `WS_TABSTOP` children and Enter / Esc → `WM_COMMAND(IDOK/IDCANCEL)` to
//!   the dialog proc.
//!
//! Deferred (documented in the structural-tier design): mnemonics, arrow-key
//! navigation, `WM_GETDLGCODE`-driven navigation, `DLGTEMPLATEEX`.

use anyhow::{Context, Result};

use super::{
    BN_CLICKED, BS_DEFPUSHBUTTON, CreateWindowRequest, GuestCallbackRequest, HandlerContext,
    IDCANCEL, QueuedWindowMessage, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_TAB, WM_COMMAND,
    WM_INITDIALOG, WM_KEYDOWN, WM_QUIT, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WindowClassIdentifier, WindowRecord,
    checked_field_address, create_window_record, find_window, find_window_mut, make_command_wparam,
    message::handle_default_window_procedure, read_guest_ansi_lossy, read_guest_u32,
    read_guest_u64, read_guest_utf16_lossy, window::deliver_focus_change, window_client_size,
    write_guest_ansi_c_string, write_guest_utf16_c_string,
};
use crate::OuterReturn;
use crate::gdi32::IRect;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::subtract_rect;
use wie_pe::resources::{DialogItemTemplate, DialogTemplate, ItemClass};

/// `GetSysColor(COLOR_BTNFACE)` — the classic dialog face color.
const DIALOG_BG: u32 = 0x00F0_F0F0;
/// `GetSysColor(COLOR_BTNSHADOW)` — the dialog border gray.
const DIALOG_BORDER: u32 = 0x00A0_A0A0;
/// Synthesized fallback dialog size (pixels) when a template id is missing.
const DEFAULT_DIALOG_CX: i32 = 300;
const DEFAULT_DIALOG_CY: i32 = 200;

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
    let init_param = read_guest_u64(engine, rsp.wrapping_add(0x28))
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
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
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

    // Modal dialogs are open: an empty GetMessage must yield, not synthesize
    // the regression-mode WM_QUIT (that would close the dialog at open).
    {
        let mut queue = state.lock_message_queue();
        queue.dialog_depth = queue.dialog_depth.saturating_add(1);
        tracing::debug!(
            target: "wiegui",
            depth = queue.dialog_depth,
            "dialog depth up"
        );
    }

    // A modal dialog takes activation (real Windows): GetActiveWindow must
    // return the dialog while it is open — guests post Enter/keys to it.
    state.window_state().active_window_handle = crate::handles::Hwnd::from(dialog_hwnd);

    // Dialogs paint on open: mark the dialog and its controls invalidated so
    // the first empty GetMessage synthesizes their WM_PAINTs.
    for hwnd in subtree {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    // Initial keyboard focus: the first WS_TABSTOP child in creation order.
    // Windows gives it focus and passes it as WM_INITDIALOG's wParam, so the
    // first keystrokes land on the dialog instead of the owner and GetFocus()
    // reads a control.
    let first_tabstop = first_tabstop_child(state, dialog_hwnd);
    if first_tabstop != 0 {
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(first_tabstop);
        // The focused control receives WM_SETFOCUS (host-side — controls have
        // no guest WndProc, so no bridge signal).
        let _ = deliver_focus_change(
            state,
            engine,
            0,
            first_tabstop,
            OuterReturn::Fixed(first_tabstop),
        )?;
    }

    if dialog_proc == 0 {
        // No dialog proc: nothing to bridge, the dialog just exists.
        let return_address = engine
            .return_from_win64_api(dialog_hwnd)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: dialog_hwnd,
        });
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

/// Resolve the `hInstance`+id dialog template, synthesizing a default dialog
/// (per the design: fallback only) when the id is missing or not addressable.
fn resolve_template(
    state: &WinApiState,
    image_base: u64,
    instance_handle: u64,
    template_value: u64,
) -> DialogTemplate {
    // Only numeric ids are addressable by DialogBoxParam. A name pointer
    // (high 16 bits nonzero) references a string-table template the parser
    // does not resolve.
    let template_id =
        (template_value >> 16 == 0).then(|| u16::try_from(template_value & 0xFFFF).unwrap_or(0));

    let dialogs: Vec<&DialogTemplate> = if instance_handle == image_base {
        state.process.main_module_dialogs.iter().collect()
    } else {
        state
            .module_state
            .loaded_modules
            .values()
            .filter(|module| module.image_base == instance_handle)
            .flat_map(|module| module.dialogs.iter())
            .collect()
    };

    template_id
        .and_then(|id| find_template(&dialogs, id))
        .cloned()
        .unwrap_or_else(synthesized_default)
}

/// Find a template by numeric id.
fn find_template<'a>(dialogs: &'a [&DialogTemplate], id: u16) -> Option<&'a DialogTemplate> {
    dialogs.iter().copied().find(|dialog| dialog.name == id)
}

/// Synthesized fallback dialog: `"WIE Dialog"`, ~300×200 px, no items.
fn synthesized_default() -> DialogTemplate {
    DialogTemplate {
        name: 0,
        style: WS_VISIBLE | WS_CLIPCHILDREN,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 150, // DEFAULT_DIALOG_CX / 2
        cy: 100, // DEFAULT_DIALOG_CY / 2
        title: "WIE Dialog".to_owned(),
        font_point: None,
        font_face: None,
        pixel_rect: wie_pe::resources::PixelRect {
            x: 0,
            y: 0,
            cx: DEFAULT_DIALOG_CX,
            cy: DEFAULT_DIALOG_CY,
        },
        items: Vec::new(),
    }
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
                .or_insert_with(|| super::controls::ControlClassKind::Button.new_state())
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

/// Paint a dialog window's face + border into the owner's surface.
///
/// The face honors the dialog's forced `WS_CLIPCHILDREN` (subtract_rect
/// decomposition, mirroring `blit.rs`), so a dialog repaint can never erase
/// its controls. Publishes the owner surface so headless frame capture sees
/// the dialog the moment it opens; control paints publish afterwards.
pub(crate) fn paint_dialog(state: &mut WinApiState, hwnd: u64) {
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    if width <= 0 || height <= 0 {
        return;
    }
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return;
    };
    // WS_CLIPCHILDREN: decompose the face rect around every visible child so
    // the face fill cannot cover a control that painted first.
    let children: Vec<IRect> = state
        .window_state()
        .windows
        .iter()
        .filter(|w| w.parent_handle == crate::handles::Hwnd::from(hwnd) && w.visible)
        .map(|w| IRect {
            left: w.x,
            top: w.y,
            right: w.x.saturating_add(w.width),
            bottom: w.y.saturating_add(w.height),
        })
        .collect();
    let mut rects = vec![IRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    }];
    for child in children {
        rects = subtract_rect(rects, child);
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
            DIALOG_BG,
        );
    }
    // 1 px border around the dialog (classic 3D shadow gray).
    if width > 2 && height > 2 {
        let right = info.offset_x.saturating_add(width).saturating_sub(1);
        let bottom = info.offset_y.saturating_add(height).saturating_sub(1);
        for rect in [
            (info.offset_x, info.offset_y, width, 1),
            (info.offset_x, bottom, width, 1),
            (info.offset_x, info.offset_y, 1, height),
            (right, info.offset_y, 1, height),
        ] {
            fill_rect_surface(
                state,
                info.hwnd,
                info.width,
                info.height,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                DIALOG_BORDER,
            );
        }
    }
    // One publish for the whole face + border (per-piece publishes would fire
    // the wake callback once per decomposed rect). B3.6: deferred — the
    // runtime drains pending publishes once per repaint cycle, so the
    // dialog face and the control paints that follow it emit one frame.
    state.present().publish_deferred(info.hwnd);
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
        tracing::info!(
            target: "wiegui",
            hwnd = dialog_hwnd,
            result = result & u64::from(u32::MAX),
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
            window.erase_background = true;
        }
        invalidate_subtree(state, crate::handles::Hwnd::from(owner));

        1
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EndDialog")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    if dialog_proc == 0 || message_address == 0 {
        // Not a dialog (or no message): caller continues normal dispatch.
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let message = read_guest_u32(
        engine,
        checked_field_address(message_address, 8, "MSG.message"),
    )
    .with_context(|| format!("failed to read MSG.message for {api_name}"))?;
    let word_parameter = read_guest_u64(
        engine,
        checked_field_address(message_address, 16, "MSG.wParam"),
    )
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
                && w.control_kind == Some(super::controls::ControlClassKind::Button)
        })
        .filter(|w| {
            ws.control_states
                .get(&w.handle)
                .is_some_and(super::controls::ControlState::is_default_push)
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
                && w.control_kind == Some(super::controls::ControlClassKind::Button)
        })
        .map(|w| w.handle.as_u64())
}

/// Whether the Shift key is held (per the guest keyboard state).
fn vk_is_shift_down(state: &WinApiState) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.keyboard_state
            .0
            .get(usize::try_from(VK_SHIFT).unwrap_or(0))
            .is_some_and(|&key| key & 0x80 != 0)
    })
}

/// Handles `USER32.dll!GetDlgItemA`.
pub fn handle_get_dlg_item_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item(ctx, "GetDlgItemA")
}
/// Handles `USER32.dll!GetDlgItemW`.
pub fn handle_get_dlg_item_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item(ctx, "GetDlgItemW")
}

fn handle_get_dlg_item(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);

    let return_address = engine
        .return_from_win64_api(child)
        .with_context(|| format!("failed to return from {api_name}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: child,
    })
}

/// Find a dialog's child control by id (control ids live in `menu_handle`).
fn get_dlg_item(state: &WinApiState, dialog_hwnd: u64, id: u16) -> u64 {
    state
        .try_window_state()
        .and_then(|ws| {
            ws.windows.iter().find(|w| {
                w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                    && w.menu_handle == u64::from(id)
            })
        })
        .map_or(0, |w| w.handle.as_u64())
}

/// Handles `USER32.dll!GetDlgItemTextA`.
pub fn handle_get_dlg_item_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_text(ctx, false, "GetDlgItemTextA")
}
/// Handles `USER32.dll!GetDlgItemTextW`.
pub fn handle_get_dlg_item_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_text(ctx, true, "GetDlgItemTextW")
}

fn handle_get_dlg_item_text(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let buffer_ptr = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let max_characters = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let text = if child == 0 {
        String::new()
    } else {
        find_window_ref(state, child).map_or_else(String::new, |w| {
            if w.control_kind.is_some() {
                w.control_text.clone()
            } else {
                w.title.clone()
            }
        })
    };

    let return_value = if text.is_empty() || buffer_ptr == 0 {
        0
    } else {
        let capacity = usize::try_from(max_characters).unwrap_or(0);
        let copied = if unicode {
            write_guest_utf16_c_string(engine, buffer_ptr, capacity, &text)?
        } else {
            write_guest_ansi_c_string(engine, buffer_ptr, capacity, &text)?
        };
        u64::try_from(copied).unwrap_or(0)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!SetDlgItemTextA`.
pub fn handle_set_dlg_item_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_dlg_item_text(ctx, false, "SetDlgItemTextA")
}
/// Handles `USER32.dll!SetDlgItemTextW`.
pub fn handle_set_dlg_item_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_dlg_item_text(ctx, true, "SetDlgItemTextW")
}

fn handle_set_dlg_item_text(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let text_ptr = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let success = child != 0 && text_ptr != 0;
    if success {
        let text = if unicode {
            read_guest_utf16_lossy(engine, text_ptr, 32_768)?
        } else {
            read_guest_ansi_lossy(engine, text_ptr, 32_768)?
        };
        if let Some(window) = find_window_mut(state, child) {
            window.control_text = text;
            window.invalidated = true;
        }
    }

    let return_value = u64::from(success);
    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
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

/// Read-only window lookup (for helpers holding `&WinApiState`).
fn find_window_ref(state: &WinApiState, handle: u64) -> Option<&WindowRecord> {
    state.try_window_state().and_then(|ws| {
        ws.windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(handle))
    })
}
