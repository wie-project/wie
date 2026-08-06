//! `FindTextW` / `ReplaceTextW` modeless find/replace dialogs.

use super::{ES_AUTOHSCROLL, WS_BORDER, resolve_dialog_owner};
use crate::guest_layout::FindReplace;
use crate::guest_memory::{with_typed_read, with_typed_write};
use crate::guest_string::{read_utf16_lossy, write_utf16_c_string};
use crate::handles::Hwnd;
use crate::state::{FindDialogSession, QueuedWindowMessage};
use crate::user32::controls::ControlClassKind;
use crate::user32::{
    BS_DEFPUSHBUTTON, CommandPayload, CreateWindowRequest, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP,
    WS_VISIBLE, WindowClassIdentifier, create_window_record, deliver_focus_change, find_window,
    find_window_mut, window_client_size,
};
use crate::{HandlerContext, OuterReturn, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

// The `FINDREPLACE` layout lives in `crate::guest_layout::FindReplace`
// (mingw-w64-verified offsets). The structure is a UNICODE structure
// regardless of the A/W suffix of the creating API.

/// `FINDREPLACE.Flags` bits (commdlg.h). `FR_DOWN` (0x1) is deliberately not
/// named: the dialog preserves the owner's down/up choice untouched.
const FR_WHOLEWORD: u32 = 0x0002;
const FR_MATCHCASE: u32 = 0x0004;
const FR_FINDNEXT: u32 = 0x0008;
const FR_REPLACE: u32 = 0x0010;
const FR_REPLACEALL: u32 = 0x0020;
const FR_DIALOGTERM: u32 = 0x0040;

/// Control ids inside the find/replace dialog.
///
/// Cancel reuses IDCANCEL's numeric value (2) so IsDialogMessage's Escape
/// handling (`DialogKeyAction::Command(IDCANCEL)`) closes the dialog without
/// a special case.
const FIND_DLG_EDIT_ID: u16 = 1200;
const FIND_DLG_REPLACE_EDIT_ID: u16 = 1201;
const FIND_DLG_MATCH_CASE_ID: u16 = 1202;
const FIND_DLG_WHOLE_WORD_ID: u16 = 1203;
const FIND_DLG_FIND_NEXT_ID: u16 = 1204;
const FIND_DLG_REPLACE_ID: u16 = 1205;
const FIND_DLG_REPLACE_ALL_ID: u16 = 1206;
const FIND_DLG_CANCEL_ID: u16 = 2;

/// Find-dialog window size (pixels). Replace mode is taller (an extra row).
const FIND_DLG_CX: i32 = 340;
const FIND_DLG_CY: i32 = 150;
const FIND_DLG_CY_REPLACE: i32 = 190;

/// Find-dialog layout (pixels, classic 8×16 base units at 96 DPI).
const FIND_DLG_LABEL_X: i32 = 8;
const FIND_DLG_FIELD_X: i32 = 92;
/// The find/replace EDITs must end before the button column (x=244) — the old
/// 232-wide field ran 80 px UNDER the Find Next button (the "buttons overlap"
/// report). 92+148=240 leaves a 4 px gap.
const FIND_DLG_FIELD_W: i32 = 148;
const FIND_DLG_EDIT_H: i32 = 22;
const FIND_DLG_BTN_X: i32 = 244;
const FIND_DLG_BTN_W: i32 = 88;
const FIND_DLG_BTN_H: i32 = 26;
/// The two checkboxes stack vertically under the field (the classic
/// comdlg32 Find dialog); `FIND_DLG_CHK_ROW_H` is the row pitch. Match case
/// at `checkbox_y`, Match whole word at `checkbox_y + FIND_DLG_CHK_ROW_H` —
/// nothing reaches the button column (x=244), so no control overlaps a button.
const FIND_DLG_CHK_X: i32 = 16;
const FIND_DLG_CHK_ROW_H: i32 = 24;
const FIND_DLG_CHK_W: i32 = 140;

/// The registered-message name `FindTextW`/`ReplaceTextW` report through.
///
/// Real comdlg32 registers `"commdlg_FindReplace"` and posts with the id it
/// returns; RNotepad registers the same name at startup and waits for that
/// id. The key is the LOWERCASED form the RegisterWindowMessage handler
/// caches under (misc.rs), so a look-up matches the guest's own
/// `RegisterWindowMessageW("commdlg_FindReplace")` — the old `"findmsgstring"`
/// never matched, so the posted messages were invisible to the guest (Find
/// Next did nothing and the guest never saw FR_DIALOGTERM).
const FINDMSGSTRING_NAME: &str = "commdlg_findreplace";
/// One past the last id `RegisterWindowMessageA/W` may return (0xFFFF); the
/// mirror of `user32::misc::REGISTERED_MESSAGE_LIMIT` for the host-side
/// fallback registration in `findmsgstring_id`.
const REGISTERED_MESSAGE_LIMIT: u32 = 0x1_0000;
// ── FindTextW / ReplaceTextW modeless dialogs ─────────────────────────────
//
// The guest (RNotepad) implements ALL find logic guest-side: it handles the
// registered `FINDMSGSTRING` message, reads the `FINDREPLACE` struct (the
// message's lParam), and drives its EDIT via EM_* messages. The host only
// supplies the modeless dialog and the message: `FindTextW`/`ReplaceTextW`
// build a "FindDialog"-class window from the existing EDIT/STATIC/BUTTON
// controls, and the dialog's buttons are handled host-side — they write the
// user's choices back into the GUEST `FINDREPLACE` struct and post
// `FINDMSGSTRING` to the owner window (the same queue the guest's main
// GetMessage loop drains, so a plain push suffices — no wake needed).

/// Handles `comdlg32.dll!FindTextW`.
pub fn handle_find_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_find_replace_text(ctx, "FindTextW", false)
}

/// Handles `comdlg32.dll!ReplaceTextW`.
pub fn handle_replace_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_find_replace_text(ctx, "ReplaceTextW", true)
}

/// Shared `FindTextW`/`ReplaceTextW` implementation: reads the guest's
/// `FINDREPLACE` struct, builds the modeless host dialog (seeding the edit
/// lines from `lpstrFindWhat` / `lpstrReplaceWith`), and returns the dialog's
/// HWND. The dialog is modeless — the guest's main loop keeps pumping, so no
/// in-guest modal loop is planted (unlike `GetOpenFileName`).
fn handle_find_replace_text(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    replace_mode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let fr_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    if fr_ptr == 0 {
        // FindTextW(NULL) fails like GetOpenFileName(NULL): no dialog.
        return ctx.finish(0);
    }

    // One typed read for the whole FINDREPLACE (the find/replace buffer
    // pointers, their lengths, the checkbox Flags, and the owner); the layout
    // lives in `crate::guest_layout::FindReplace`.
    let fr = with_typed_read::<FindReplace, _, _>(engine, fr_ptr, |fr| Ok(*fr))
        .with_context(|| format!("failed to read FINDREPLACE for {api_name}"))?;
    let owner_raw = fr.hwnd_owner;
    let find_what_ptr = fr.lpstr_find_what;
    let find_what_len = u32::from(fr.w_find_what_len);
    let replace_with_ptr = if replace_mode {
        fr.lpstr_replace_with
    } else {
        0
    };
    let replace_with_len = if replace_mode {
        u32::from(fr.w_replace_with_len)
    } else {
        0
    };
    let flags = fr.flags;

    // The dialog's EDIT lines are seeded from the guest's buffers (RNotepad
    // keeps its search/replace text there across Find menu opens).
    let seed_find = if find_what_ptr != 0 {
        read_utf16_lossy(engine, find_what_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} find text"))?
    } else {
        String::new()
    };
    let seed_replace = if replace_with_ptr != 0 {
        read_utf16_lossy(engine, replace_with_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} replace text"))?
    } else {
        String::new()
    };

    let cy = if replace_mode {
        FIND_DLG_CY_REPLACE
    } else {
        FIND_DLG_CY
    };
    let parent_handle = resolve_dialog_owner(state, owner_raw);
    let (owner_w, owner_h) = window_client_size(state, parent_handle);
    let (dialog_x, dialog_y) = if parent_handle != 0 && owner_w >= FIND_DLG_CX && owner_h >= cy {
        (
            owner_w.saturating_sub(FIND_DLG_CX).saturating_div(2),
            owner_h.saturating_sub(cy).saturating_div(2),
        )
    } else {
        (0, 0)
    };

    let (dialog_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("FindDialog".to_owned()),
            title: if replace_mode {
                "Replace".to_owned()
            } else {
                "Find".to_owned()
            },
            style: WS_VISIBLE | WS_CLIPCHILDREN,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x: dialog_x,
            y: dialog_y,
            width: FIND_DLG_CX,
            height: cy,
        },
        true,
    )?;
    if dialog_hwnd == 0 {
        return ctx.finish(0);
    }

    // Row 0: "Find what:" label + search EDIT.
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Find what:".to_owned(),
        0,
        0,
        FIND_DLG_LABEL_X,
        8,
        78,
        20,
    )?;
    let find_edit_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0081), // EDIT
        seed_find,
        WS_BORDER | ES_AUTOHSCROLL,
        u64::from(FIND_DLG_EDIT_ID),
        FIND_DLG_FIELD_X,
        8,
        FIND_DLG_FIELD_W,
        FIND_DLG_EDIT_H,
    )?;

    // Replace mode adds a "Replace with:" row between the edit and the
    // checkboxes; the buttons get two extra slots.
    let replace_edit_hwnd = if replace_mode {
        create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0082), // STATIC
            "Replace with:".to_owned(),
            0,
            0,
            FIND_DLG_LABEL_X,
            36,
            78,
            20,
        )?;
        create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0081), // EDIT
            seed_replace,
            WS_BORDER | ES_AUTOHSCROLL,
            u64::from(FIND_DLG_REPLACE_EDIT_ID),
            FIND_DLG_FIELD_X,
            36,
            FIND_DLG_FIELD_W,
            FIND_DLG_EDIT_H,
        )?
    } else {
        0
    };

    // Checkboxes render as push buttons whose caption shows the state; the
    // checked state itself lives on the session and is mirrored into
    // `FINDREPLACE.Flags` when the dialog submits (the host Button control
    // has no BS_AUTOCHECKBOX state).
    let match_case_checked = flags & FR_MATCHCASE != 0;
    let whole_word_checked = flags & FR_WHOLEWORD != 0;
    let checkbox_y = if replace_mode { 64 } else { 40 };
    let match_case_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if match_case_checked {
            "[x] Match case".to_owned()
        } else {
            "[ ] Match case".to_owned()
        },
        0,
        u64::from(FIND_DLG_MATCH_CASE_ID),
        FIND_DLG_CHK_X,
        checkbox_y,
        100,
        20,
    )?;
    let whole_word_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if whole_word_checked {
            "[x] Match whole word".to_owned()
        } else {
            "[ ] Match whole word".to_owned()
        },
        0,
        u64::from(FIND_DLG_WHOLE_WORD_ID),
        FIND_DLG_CHK_X,
        checkbox_y.saturating_add(FIND_DLG_CHK_ROW_H),
        FIND_DLG_CHK_W,
        20,
    )?;

    // The command buttons: Find Next is the Enter default (BS_DEFPUSHBUTTON).
    let (find_next_y, cancel_y) = if replace_mode { (8, 98) } else { (8, 40) };
    let find_next_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Find Next".to_owned(),
        BS_DEFPUSHBUTTON,
        u64::from(FIND_DLG_FIND_NEXT_ID),
        FIND_DLG_BTN_X,
        find_next_y,
        FIND_DLG_BTN_W,
        FIND_DLG_BTN_H,
    )?;
    let replace_hwnd = if replace_mode {
        Some(create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0080), // BUTTON
            "Replace".to_owned(),
            0,
            u64::from(FIND_DLG_REPLACE_ID),
            FIND_DLG_BTN_X,
            38,
            FIND_DLG_BTN_W,
            FIND_DLG_BTN_H,
        )?)
    } else {
        None
    };
    let replace_all_hwnd = if replace_mode {
        Some(create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0080), // BUTTON
            "Replace All".to_owned(),
            0,
            u64::from(FIND_DLG_REPLACE_ALL_ID),
            FIND_DLG_BTN_X,
            68,
            FIND_DLG_BTN_W,
            FIND_DLG_BTN_H,
        )?)
    } else {
        None
    };
    let cancel_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Cancel".to_owned(),
        0,
        u64::from(FIND_DLG_CANCEL_ID),
        FIND_DLG_BTN_X,
        cancel_y,
        FIND_DLG_BTN_W,
        FIND_DLG_BTN_H,
    )?;

    // The Find Next button is the dialog's Enter default.
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(find_next_hwnd))
        .or_insert_with(|| ControlClassKind::Button.new_state())
        .set_default_push(true);

    // Record the session for the button write-back. The dialog is modeless,
    // so the message-queue dialog_depth is NOT touched: an empty GetMessage
    // keeps yielding normally while the guest pumps its main loop.
    state.window_state().find_dialogs.push(FindDialogSession {
        dialog_hwnd,
        fr_ptr,
        owner_hwnd: parent_handle,
        find_what_ptr,
        find_what_len,
        replace_with_ptr,
        replace_with_len,
        find_edit_hwnd,
        replace_edit_hwnd,
        match_case_hwnd,
        whole_word_hwnd,
        match_case_checked,
        whole_word_checked,
        replace_mode,
    });

    // The dialog takes activation and the search EDIT takes keyboard focus,
    // so the first keystrokes land in the find field.
    state.window_state().active_window_handle = Hwnd::from(dialog_hwnd);
    state.window_state().focus_window_handle = Hwnd::from(find_edit_hwnd);
    let _unused = deliver_focus_change(
        state,
        engine,
        0,
        find_edit_hwnd,
        OuterReturn::Fixed(find_edit_hwnd),
    )?;

    // Mark the whole subtree invalidated so the first empty GetMessage
    // synthesizes the WM_PAINTs (dialog face + every control).
    let mut subtree = vec![
        dialog_hwnd,
        find_edit_hwnd,
        match_case_hwnd,
        whole_word_hwnd,
        find_next_hwnd,
        cancel_hwnd,
    ];
    if replace_edit_hwnd != 0 {
        subtree.push(replace_edit_hwnd);
    }
    if let Some(hwnd) = replace_hwnd {
        subtree.push(hwnd);
    }
    if let Some(hwnd) = replace_all_hwnd {
        subtree.push(hwnd);
    }
    for hwnd in subtree {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    tracing::info!(
        target: "wiegui",
        api = api_name,
        hwnd = dialog_hwnd,
        parent = parent_handle,
        replace_mode,
        "find dialog opened"
    );

    ctx.finish(dialog_hwnd)
}

/// Create one control inside the find dialog and set its client rect.
///
/// `rect` is `(x, y, width, height)` in dialog-client pixels.
#[allow(clippy::too_many_arguments)] // 8 finder args + state; a rect struct would hide the layout
pub(crate) fn create_find_control(
    state: &mut WinApiState,
    dialog_hwnd: u64,
    class_identifier: WindowClassIdentifier,
    title: String,
    style_bits: u32,
    id: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<u64> {
    let (hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | style_bits,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: id,
            instance_handle: 0,
            x,
            y,
            width,
            height,
        },
        true,
    )?;
    if let Some(window) = find_window_mut(state, hwnd) {
        window.client_rect = (0, 0, width, height);
    }
    Ok(hwnd)
}

/// Whether `hwnd` is one of the host-owned Find/Replace dialogs.
///
/// The user32 control/message paths consult this to route the dialog's
/// messages host-side instead of bridging to a guest proc (the find dialog
/// has no `dialog_proc`).
#[must_use]
pub(crate) fn is_find_dialog_window(state: &WinApiState, hwnd: u64) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.find_dialogs
            .iter()
            .any(|session| session.dialog_hwnd == hwnd)
    })
}

/// Handle a `WM_COMMAND` raised by one of the find dialog's buttons.
///
/// Called from the user32 control dispatch (`deliver_command`) when the
/// command bubbles up to a find-dialog window — and from IsDialogMessage for
/// Enter/Escape. Buttons either toggle a checkbox or submit the dialog:
/// write the edit texts + checkbox state back into the guest `FINDREPLACE`
/// struct and post `FINDMSGSTRING` to the owner. Cancel additionally tears
/// the dialog down (FR_DIALOGTERM first, so the owner cleans up its state).
pub(crate) fn handle_find_dialog_command(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    dialog_hwnd: u64,
    word_parameter: u64,
    child_hwnd: u64,
) -> Result<Option<u64>> {
    let Some(index) = state
        .window_state()
        .find_dialogs
        .iter()
        .position(|session| session.dialog_hwnd == dialog_hwnd)
    else {
        return Ok(Some(0));
    };
    let command = CommandPayload::decode(word_parameter, child_hwnd);

    match command.id {
        FIND_DLG_MATCH_CASE_ID => {
            toggle_find_checkbox(state, index, "Match case")?;
        }
        FIND_DLG_WHOLE_WORD_ID => {
            toggle_find_checkbox(state, index, "Match whole word")?;
        }
        FIND_DLG_FIND_NEXT_ID => submit_find_dialog(engine, state, index, FR_FINDNEXT)?,
        FIND_DLG_REPLACE_ID => submit_find_dialog(engine, state, index, FR_REPLACE)?,
        FIND_DLG_REPLACE_ALL_ID => submit_find_dialog(engine, state, index, FR_REPLACEALL)?,
        FIND_DLG_CANCEL_ID => close_find_dialog(engine, state, index)?,
        _ => {
            tracing::debug!(
                target: "wiegui",
                id = command.id,
                hwnd = dialog_hwnd,
                "find dialog: unhandled command"
            );
        }
    }
    Ok(Some(0))
}

/// Toggle one of the find dialog's checkbox buttons and repaint it (the
/// caption carries the state: `[x]` / `[ ]`).
fn toggle_find_checkbox(state: &mut WinApiState, index: usize, label: &str) -> Result<()> {
    let (checked, checkbox_hwnd, title) = {
        let session = state
            .window_state()
            .find_dialogs
            .get_mut(index)
            .context("find-dialog session vanished")?;
        let checked = if label == "Match case" {
            session.match_case_checked = !session.match_case_checked;
            session.match_case_checked
        } else {
            session.whole_word_checked = !session.whole_word_checked;
            session.whole_word_checked
        };
        let checkbox_hwnd = if label == "Match case" {
            session.match_case_hwnd
        } else {
            session.whole_word_hwnd
        };
        let title = format!("[{}] {label}", if checked { "x" } else { " " });
        (checked, checkbox_hwnd, title)
    };
    tracing::debug!(target: "wiegui", checked, "find dialog checkbox toggled");
    if let Some(window) = find_window_mut(state, checkbox_hwnd) {
        window.control_text = title;
        window.invalidated = true;
    }
    Ok(())
}

/// Submit a Find/Replace/Replace-All action: write the edit texts and the
/// checkbox state into the guest `FINDREPLACE` struct, then post
/// `FINDMSGSTRING` to the owner (lParam = the struct's VA).
fn submit_find_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    index: usize,
    action_flag: u32,
) -> Result<()> {
    let session = state
        .window_state()
        .find_dialogs
        .get(index)
        .cloned()
        .context("find-dialog session vanished")?;

    let find_text = find_window(state, session.find_edit_hwnd)
        .map_or_else(String::new, |window| window.control_text.clone());
    let replace_text = if session.replace_edit_hwnd != 0 {
        find_window(state, session.replace_edit_hwnd)
            .map_or_else(String::new, |window| window.control_text.clone())
    } else {
        String::new()
    };

    // Preserve the owner's bits (FR_DOWN, FR_HIDEWHOLEWORD, ...) and fold the
    // checkbox state + the action into the action-bit group. FINDREPLACE is an
    // in/out struct: snapshot it (Copy), edit Flags, write it back untouched
    // otherwise — the MENUITEMINFO pattern.
    let mut fr = with_typed_read::<FindReplace, _, _>(engine, session.fr_ptr, |fr| Ok(*fr))
        .context("failed to read FINDREPLACE on submit")?;
    let mut new_flags = fr.flags
        & !(FR_FINDNEXT | FR_REPLACE | FR_REPLACEALL | FR_DIALOGTERM | FR_MATCHCASE | FR_WHOLEWORD);
    new_flags |= action_flag;
    if session.match_case_checked {
        new_flags |= FR_MATCHCASE;
    }
    if session.whole_word_checked {
        new_flags |= FR_WHOLEWORD;
    }

    let find_len = usize::try_from(session.find_what_len)
        .context("FINDREPLACE.wFindWhatLen does not fit usize")?;
    write_utf16_c_string(engine, session.find_what_ptr, find_len, &find_text)
        .context("failed to write FINDREPLACE.lpstrFindWhat")?;
    if session.replace_edit_hwnd != 0 {
        let replace_len = usize::try_from(session.replace_with_len)
            .context("FINDREPLACE.wReplaceWithLen does not fit usize")?;
        write_utf16_c_string(engine, session.replace_with_ptr, replace_len, &replace_text)
            .context("failed to write FINDREPLACE.lpstrReplaceWith")?;
    }
    fr.flags = new_flags;
    with_typed_write::<FindReplace, _, _>(engine, session.fr_ptr, |fr_view| {
        *fr_view = fr;
        Ok(())
    })
    .context("failed to write FINDREPLACE on submit")?;

    post_find_msgstring(state, &session);
    Ok(())
}

/// Close the find dialog: post `FINDMSGSTRING` with `FR_DIALOGTERM` so the
/// owner clears its find state, then tear the dialog subtree down.
fn close_find_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    index: usize,
) -> Result<()> {
    let session = state
        .window_state()
        .find_dialogs
        .get(index)
        .cloned()
        .context("find-dialog session vanished")?;

    let mut fr = with_typed_read::<FindReplace, _, _>(engine, session.fr_ptr, |fr| Ok(*fr))
        .context("failed to read FINDREPLACE on close")?;
    fr.flags =
        (fr.flags & !(FR_FINDNEXT | FR_REPLACE | FR_REPLACEALL | FR_DIALOGTERM)) | FR_DIALOGTERM;
    with_typed_write::<FindReplace, _, _>(engine, session.fr_ptr, |fr_view| {
        *fr_view = fr;
        Ok(())
    })
    .context("failed to write FINDREPLACE on close")?;

    post_find_msgstring(state, &session);
    destroy_find_dialog(state, &session);
    Ok(())
}

/// Queue `FINDMSGSTRING` to the dialog's owner with lParam = the guest
/// `FINDREPLACE` VA. The owner's GetMessage loop is already pumping (modeless
/// dialog), so a plain queue push delivers it on the next drain.
fn post_find_msgstring(state: &mut WinApiState, session: &FindDialogSession) {
    let message_id = findmsgstring_id(state);
    let mut queue = state.lock_message_queue();
    let time = queue.next_message_time;
    queue.messages.push(QueuedWindowMessage {
        window_handle: Hwnd::from(session.owner_hwnd),
        message: message_id,
        word_parameter: 0,
        long_parameter: session.fr_ptr,
        time,
        point_x: 0,
        point_y: 0,
    });
    queue.next_message_time = time.saturating_add(1);
    tracing::debug!(
        target: "wiegui",
        message_id,
        owner = session.owner_hwnd,
        fr_ptr = session.fr_ptr,
        "FINDMSGSTRING posted"
    );
}

/// Resolve the id `RegisterWindowMessageW("FINDMSGSTRING")` returns — or, if
/// the guest never registered it, allocate one now (registered names are
/// session-stable, so a later guest registration returns the same id).
fn findmsgstring_id(state: &mut WinApiState) -> u32 {
    if let Some(id) = state
        .window_state()
        .registered_messages
        .get(FINDMSGSTRING_NAME)
        .copied()
    {
        return id;
    }
    if state.window_state().next_registered_message >= REGISTERED_MESSAGE_LIMIT {
        return 0;
    }
    let id = state.window_state().next_registered_message;
    state.window_state().next_registered_message = state
        .window_state()
        .next_registered_message
        .saturating_add(1);
    state
        .window_state()
        .registered_messages
        .insert(FINDMSGSTRING_NAME.to_owned(), id);
    id
}

/// Remove the find dialog, its controls, and its session state.
fn destroy_find_dialog(state: &mut WinApiState, session: &FindDialogSession) {
    let dialog_handle = Hwnd::from(session.dialog_hwnd);
    // Snapshot (handle, parent) so the subtree walk does not re-borrow
    // `state` while the window list is being iterated.
    let pairs: Vec<(Hwnd, Hwnd)> = state
        .window_state()
        .windows
        .iter()
        .map(|window| (window.handle, window.parent_handle))
        .collect();
    let subtree: Vec<Hwnd> = pairs
        .iter()
        .filter_map(|(handle, _)| {
            if window_handle_in_subtree(&pairs, *handle, dialog_handle) {
                Some(*handle)
            } else {
                None
            }
        })
        .collect();
    state
        .window_state()
        .windows
        .retain(|window| !subtree.contains(&window.handle));
    state
        .window_state()
        .control_states
        .retain(|hwnd, _| !subtree.contains(hwnd));
    if subtree.contains(&state.window_state().focus_window_handle) {
        state.window_state().focus_window_handle = Hwnd::NULL;
    }
    if subtree.contains(&state.window_state().capture_window_handle) {
        state.window_state().capture_window_handle = Hwnd::NULL;
    }
    if subtree.contains(&state.window_state().active_window_handle) {
        state.window_state().active_window_handle = Hwnd::NULL;
    }
    state
        .window_state()
        .find_dialogs
        .retain(|candidate| candidate.dialog_hwnd != session.dialog_hwnd);
    // The dialog composites into the owner surface (it has no winit window of
    // its own), so removing its subtree leaves the face pixels behind in the
    // owner frame. Mark the owner's whole remaining subtree invalidated: the
    // next empty GetMessage repaints the owner + controls over the stale face
    // (mirrors the font-dialog open pattern at comdlg32/font.rs).
    let owner_handle = Hwnd::from(session.owner_hwnd);
    if owner_handle != Hwnd::NULL {
        let pairs: Vec<(Hwnd, Hwnd)> = state
            .window_state()
            .windows
            .iter()
            .map(|window| (window.handle, window.parent_handle))
            .collect();
        let owner_subtree: Vec<Hwnd> = pairs
            .iter()
            .filter_map(|(handle, _)| {
                if window_handle_in_subtree(&pairs, *handle, owner_handle) {
                    Some(*handle)
                } else {
                    None
                }
            })
            .collect();
        for hwnd in owner_subtree {
            if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
                window.invalidated = true;
            }
        }
    }
    tracing::info!(
        target: "wiegui",
        hwnd = session.dialog_hwnd,
        "find dialog closed"
    );
}

/// Whether `handle` is `root` or a descendant of `root` (parent-chain walk
/// over a `(handle, parent)` snapshot).
fn window_handle_in_subtree(pairs: &[(Hwnd, Hwnd)], handle: Hwnd, root: Hwnd) -> bool {
    if handle == root {
        return true;
    }
    let mut current = handle;
    loop {
        let Some(&(_, parent)) = pairs.iter().find(|(candidate, _)| *candidate == current) else {
            return false;
        };
        if parent == root {
            return true;
        }
        if parent == Hwnd::NULL || parent == current {
            return false;
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        handle_find_dialog_command, handle_find_text_w, handle_replace_text_w,
        is_find_dialog_window,
    };
    use crate::comdlg32::test_support::{
        create_owner_window, read_guest_u32_at, read_guest_utf16, test_engine, test_environment,
        test_state, utf16_bytes, write_regs,
    };
    use crate::handles::Hwnd;
    use crate::user32::{BN_CLICKED, find_window, find_window_mut, make_command_wparam};
    use crate::{HandlerContext, WinApiState};
    use wie_cpu::{CpuEngine, IcedCpu};

    // ── FindTextW / ReplaceTextW (Task 4.2) ───────────────────────────────

    const FR_TEST_DOWN: u32 = 0x0001;
    const FR_TEST_MATCHCASE: u32 = 0x0004;
    const FR_TEST_FINDNEXT: u32 = 0x0008;
    const FR_TEST_REPLACE: u32 = 0x0010;
    const FR_TEST_REPLACEALL: u32 = 0x0020;
    const FR_TEST_DIALOGTERM: u32 = 0x0040;

    /// Write a `FINDREPLACE` (Win64) into guest memory at `fr_ptr`, with the
    /// find/replace string buffers at the fixed test addresses 0x6000/0x6100.
    fn write_findreplace(engine: &mut IcedCpu, fr_ptr: u64, owner: u64, flags: u32) {
        engine.mem_write(fr_ptr, &0x58_u32.to_le_bytes()).ok(); // lStructSize
        engine.mem_write(fr_ptr + 8, &owner.to_le_bytes()).ok(); // hwndOwner
        engine.mem_write(fr_ptr + 16, &0_u64.to_le_bytes()).ok(); // hInstance
        engine.mem_write(fr_ptr + 24, &flags.to_le_bytes()).ok(); // Flags
        engine
            .mem_write(fr_ptr + 32, &0x6000_u64.to_le_bytes())
            .ok(); // lpstrFindWhat
        engine
            .mem_write(fr_ptr + 40, &0x6100_u64.to_le_bytes())
            .ok(); // lpstrReplaceWith
        // The lengths are WORDs at 48/50 (lCustData needs 8-alignment, so the
        // real layout pads 52..56) — the typed read enforces the header's
        // offsets, unlike the old per-field constants.
        engine.mem_write(fr_ptr + 48, &64_u16.to_le_bytes()).ok(); // wFindWhatLen
        engine.mem_write(fr_ptr + 50, &64_u16.to_le_bytes()).ok(); // wReplaceWithLen
    }

    /// Open a find (or replace) dialog the way the guest would: `FindTextW`
    /// / `ReplaceTextW` with a seeded `FINDREPLACE`; returns the dialog hwnd.
    fn open_find_dialog(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        owner_hwnd: u64,
        replace: bool,
    ) -> u64 {
        let find_buf = 0x6000;
        let replace_buf = 0x6100;
        engine.mem_write(find_buf, &utf16_bytes("needle")).ok();
        engine.mem_write(replace_buf, &utf16_bytes("haystack")).ok();
        write_findreplace(engine, 0x5000, owner_hwnd, FR_TEST_DOWN | FR_TEST_MATCHCASE);
        write_regs(engine, 0x5000, 0, 0, 0);
        let result = if replace {
            handle_replace_text_w(&mut HandlerContext::new(engine, test_environment(), state))
        } else {
            handle_find_text_w(&mut HandlerContext::new(engine, test_environment(), state))
        }
        .expect("find dialog opens");
        result.return_value
    }

    /// The queued FINDMSGSTRING, if one was posted.
    fn queued_find_msg(state: &mut WinApiState) -> Option<crate::state::QueuedWindowMessage> {
        let findmsg_id = state
            .window_state()
            .registered_messages
            .get(super::FINDMSGSTRING_NAME)
            .copied();
        let queue = state.lock_message_queue();
        findmsg_id.and_then(|id| {
            queue
                .messages
                .iter()
                .find(|message| message.message == id)
                .cloned()
        })
    }

    #[test]
    fn find_text_w_builds_modeless_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        assert_ne!(dialog_hwnd, 0, "FindTextW returns the dialog HWND");

        let session = state
            .window_state()
            .find_dialogs
            .first()
            .expect("session recorded")
            .clone();
        assert_eq!(session.dialog_hwnd, dialog_hwnd);
        assert_eq!(session.owner_hwnd, owner_hwnd);
        assert_eq!(session.fr_ptr, 0x5000);
        assert!(!session.replace_mode);
        assert_eq!(session.find_what_ptr, 0x6000);
        assert_eq!(session.find_what_len, 64);

        // The find EDIT is seeded from lpstrFindWhat and takes the focus.
        assert_eq!(
            session.find_edit_hwnd,
            state.window_state().focus_window_handle.as_u64()
        );
        let edit = find_window(&mut state, session.find_edit_hwnd).expect("find edit");
        assert_eq!(edit.control_text, "needle");
        // The checkbox state mirrors the guest's initial Flags (FR_MATCHCASE).
        assert!(session.match_case_checked);
        assert!(!session.whole_word_checked);

        // Modeless: the message-queue dialog depth is untouched and the
        // dialog + its controls exist as ordinary windows.
        assert_eq!(state.lock_message_queue().dialog_depth, 0);
        assert!(
            find_window(&mut state, dialog_hwnd).is_some(),
            "dialog window exists"
        );
        let controls = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(dialog_hwnd))
            .count();
        assert_eq!(controls, 6, "label + edit + 2 checkboxes + 2 buttons");
    }

    #[test]
    fn find_text_w_null_returns_zero() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0, 0, 0, 0);
        let result = handle_find_text_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("NULL FindTextW returns zero");
        assert_eq!(result.return_value, 0);
        assert!(state.window_state().find_dialogs.is_empty());
    }

    #[test]
    fn find_next_writes_struct_and_posts_findmsgstring() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        // The guest registered FINDMSGSTRING at startup (like RNotepad does).
        state
            .window_state()
            .registered_messages
            .insert(super::FINDMSGSTRING_NAME.to_owned(), 0xC100);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        let find_next_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_FIND_NEXT_ID))
            .expect("Find Next button")
            .handle
            .as_u64();

        // The user typed a new search string into the find EDIT.
        if let Some(window) = find_window_mut(&mut state, session.find_edit_hwnd) {
            window.control_text = "needle2".to_owned();
        }

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_FIND_NEXT_ID), BN_CLICKED),
            find_next_hwnd,
        )
        .expect("Find Next handled");

        // The new text lands in the guest lpstrFindWhat buffer.
        assert_eq!(read_guest_utf16(&mut engine, 0x6000, 32), "needle2");
        // Flags: FR_FINDNEXT set, FR_MATCHCASE preserved from the checkbox,
        // FR_DOWN preserved from the guest.
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_eq!(
            flags & (FR_TEST_FINDNEXT | FR_TEST_MATCHCASE | FR_TEST_DOWN),
            FR_TEST_FINDNEXT | FR_TEST_MATCHCASE | FR_TEST_DOWN,
            "action + checkbox + preserved bits"
        );
        assert_eq!(flags & FR_TEST_REPLACE, 0);
        // FINDMSGSTRING posted to the owner with lParam = the FINDREPLACE VA.
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.message, 0xC100);
        assert_eq!(posted.long_parameter, 0x5000);
        assert_eq!(posted.word_parameter, 0);
        // The dialog stays open (modeless).
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn checkbox_toggle_updates_state_and_caption() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(session.match_case_checked);

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_MATCH_CASE_ID), BN_CLICKED),
            session.match_case_hwnd,
        )
        .expect("checkbox toggle handled");

        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(!session.match_case_checked);
        let checkbox = find_window(&mut state, session.match_case_hwnd).expect("checkbox");
        assert_eq!(checkbox.control_text, "[ ] Match case");
        // No FINDMSGSTRING is posted for a checkbox toggle.
        assert!(state.lock_message_queue().messages.is_empty());
    }

    #[test]
    fn cancel_posts_dialogterm_and_destroys_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let window_count = state.window_state().windows.len();
        let cancel_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_CANCEL_ID))
            .expect("Cancel button")
            .handle
            .as_u64();

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_CANCEL_ID), BN_CLICKED),
            cancel_hwnd,
        )
        .expect("Cancel handled");

        // FR_DIALOGTERM posted (the fallback registration allocates the id).
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_DIALOGTERM, 0);
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.long_parameter, 0x5000);
        // The dialog + its 6 controls are gone; only the owner remains.
        assert_eq!(state.window_state().windows.len(), window_count - 7);
        assert!(state.window_state().find_dialogs.is_empty());
        assert!(
            state
                .window_state()
                .windows
                .iter()
                .all(|w| w.handle.as_u64() != dialog_hwnd)
        );
    }

    #[test]
    fn replace_text_w_builds_replace_dialog_and_submits() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert(super::FINDMSGSTRING_NAME.to_owned(), 0xC100);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(session.replace_mode);
        assert_ne!(session.replace_edit_hwnd, 0);
        let replace_edit =
            find_window(&mut state, session.replace_edit_hwnd).expect("replace edit");
        assert_eq!(replace_edit.control_text, "haystack");

        // The user typed into both edits, then pressed Replace.
        if let Some(window) = find_window_mut(&mut state, session.find_edit_hwnd) {
            window.control_text = "a".to_owned();
        }
        if let Some(window) = find_window_mut(&mut state, session.replace_edit_hwnd) {
            window.control_text = "b".to_owned();
        }
        let replace_button = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_REPLACE_ID))
            .expect("Replace button")
            .handle
            .as_u64();
        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_REPLACE_ID), BN_CLICKED),
            replace_button,
        )
        .expect("Replace handled");

        assert_eq!(read_guest_utf16(&mut engine, 0x6000, 32), "a");
        assert_eq!(read_guest_utf16(&mut engine, 0x6100, 32), "b");
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_REPLACE, 0);
        assert_eq!(flags & FR_TEST_REPLACEALL, 0);
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.long_parameter, 0x5000);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn replace_all_sets_replaceall_flag() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert(super::FINDMSGSTRING_NAME.to_owned(), 0xC100);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);
        let replace_all_button = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_REPLACE_ALL_ID))
            .expect("Replace All button")
            .handle
            .as_u64();

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_REPLACE_ALL_ID), BN_CLICKED),
            replace_all_button,
        )
        .expect("Replace All handled");

        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_REPLACEALL, 0);
        assert_eq!(flags & FR_TEST_REPLACE, 0);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn find_next_button_click_flows_through_control_dispatch() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert(super::FINDMSGSTRING_NAME.to_owned(), 0xC100);
        open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let find_next_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_FIND_NEXT_ID))
            .expect("Find Next button")
            .handle
            .as_u64();

        // A real mouse click: the guest's DispatchMessage routes the press
        // and release to the Button control, whose WM_LBUTTONUP delivers
        // BN_CLICKED to the find dialog (host-side, via deliver_button_command).
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            find_next_hwnd,
            crate::user32::WinMsg::WM_LBUTTONDOWN.as_u32(),
            0,
            0,
        )
        .expect("button press");
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            find_next_hwnd,
            crate::user32::WinMsg::WM_LBUTTONUP.as_u32(),
            0,
            0,
        )
        .expect("button release");

        // The click submitted the dialog: FINDMSGSTRING to the owner with
        // FR_FINDNEXT set, and the dialog stays open (modeless).
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted via click");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.long_parameter, 0x5000);
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_FINDNEXT, 0);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn find_and_replace_dialogs_coexist() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let find_dialog = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let replace_dialog = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);

        assert_ne!(find_dialog, replace_dialog);
        assert_eq!(state.window_state().find_dialogs.len(), 2);
        // Each dialog is independently detectable.
        assert!(is_find_dialog_window(&state, find_dialog));
        assert!(is_find_dialog_window(&state, replace_dialog));
        assert!(!is_find_dialog_window(&state, owner_hwnd));
    }

    #[test]
    fn is_find_dialog_window_false_when_closed() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        assert!(is_find_dialog_window(&state, dialog_hwnd));
        // After the dialog is destroyed the session is gone.
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_CANCEL_ID), BN_CLICKED),
            session.match_case_hwnd,
        )
        .expect("cancel");
        assert!(!is_find_dialog_window(&state, dialog_hwnd));
    }

    /// The "Find Next does nothing" + "cannot reopen" root cause: the dialog
    /// must post FINDMSGSTRING with the SAME id the guest registered for
    /// `"commdlg_FindReplace"`. RNotepad registers that name at startup
    /// (id 0xC000 in the trace); the old `"findmsgstring"` name missed the
    /// lookup and posted a fresh id the guest never received.
    #[test]
    fn findmsgstring_id_resolves_the_guest_registered_message_id() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        // The guest registers "commdlg_FindReplace" at startup, exactly like
        // RNotepad — through the real RegisterWindowMessageW handler, so the
        // id + lowercased-key caching are the production ones.
        let name_addr = 0x7000;
        engine
            .mem_write(name_addr, &utf16_bytes("commdlg_FindReplace"))
            .ok();
        write_regs(&mut engine, name_addr, 0, 0, 0);
        let result = crate::user32::handle_register_window_message_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("guest registers commdlg_FindReplace");
        let registered_id = result.return_value;
        assert_ne!(registered_id, 0, "registration succeeds");
        assert_eq!(
            registered_id, 0xC000,
            "first registered id in a fresh session"
        );

        // Open the find dialog and click Find Next.
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let find_next_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_FIND_NEXT_ID))
            .expect("Find Next button")
            .handle
            .as_u64();
        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_FIND_NEXT_ID), BN_CLICKED),
            find_next_hwnd,
        )
        .expect("Find Next handled");

        // The posted message id IS the guest's registered id — not a fallback.
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(
            posted.message,
            u32::try_from(registered_id).unwrap_or(0),
            "FINDMSGSTRING must be posted with the guest's registered id \
             (0x{registered_id:x}) — a different id is invisible to the guest"
        );
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        // No second registration was allocated: the map has exactly one entry
        // and it is the guest's.
        assert_eq!(state.window_state().registered_messages.len(), 1);
    }

    /// The "buttons overlap" root cause, pinned: no two controls of a find or
    /// replace dialog may overlap, and every control must fit inside the
    /// dialog client rect. The old layout ran the find EDIT 80 px under the
    /// Find Next button and the Match-whole-word checkbox over the Cancel
    /// button (both end past x=244, where the button column starts).
    #[test]
    fn find_dialog_controls_do_not_overlap() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        for replace in [false, true] {
            let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, replace);
            let dialog = find_window(&mut state, dialog_hwnd).expect("dialog window");
            let (dlg_w, dlg_h) = (dialog.width, dialog.height);
            let controls: Vec<(u64, i32, i32, i32, i32)> = state
                .window_state()
                .windows
                .iter()
                .filter(|w| w.parent_handle == Hwnd::from(dialog_hwnd))
                .map(|w| (w.handle.as_u64(), w.x, w.y, w.width, w.height))
                .collect();
            assert!(
                controls.len() >= 6,
                "find dialog must have its controls, got {}",
                controls.len()
            );

            for (hwnd, x, y, cx, cy) in &controls {
                assert!(
                    *x >= 0 && *y >= 0 && *cx > 0 && *cy > 0,
                    "control {hwnd} must have a positive in-dialog rect"
                );
                assert!(
                    x.saturating_add(*cx) <= dlg_w && y.saturating_add(*cy) <= dlg_h,
                    "control {hwnd} ({x},{y} {cx}x{cy}) must fit inside the \
                     {dlg_w}x{dlg_h} dialog — it hangs out of the window"
                );
            }
            for (a, ax, ay, acx, acy) in &controls {
                for (b, bx, by, bcx, bcy) in &controls {
                    if a == b {
                        continue;
                    }
                    let overlap_x = ax < &bx.saturating_add(*bcx) && bx < &ax.saturating_add(*acx);
                    let overlap_y = ay < &by.saturating_add(*bcy) && by < &ay.saturating_add(*acy);
                    assert!(
                        !(overlap_x && overlap_y),
                        "controls {a} and {b} overlap in the {} dialog \
                         ({ax},{ay} {acx}x{acy} vs {bx},{by} {bcx}x{bcy})",
                        if replace { "replace" } else { "find" }
                    );
                }
            }
        }
    }
}
