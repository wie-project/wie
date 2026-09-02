//! `ChooseFontW` interactive font dialog.

use super::{resolve_dialog_owner, state_comm_dlg_none};
use crate::comdlg32::find::create_find_control;
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::{ChooseFontW, LogFontW};
use crate::guest_memory::{
    checked_address, read_i32, read_u32, read_u64, with_typed_read, with_typed_write,
};
use crate::guest_string::read_utf16_lossy;
use crate::handles::Hwnd;
use crate::state::FontDialogSession;
use crate::user32::controls::{ControlClassKind, ControlState};
use crate::user32::{
    BS_DEFPUSHBUTTON, CreateWindowRequest, GuestCallbackRequest, IDCANCEL, IDOK, ModalFrame,
    WS_CLIPCHILDREN, WS_VISIBLE, WinApiControlSignal, WindowClassIdentifier, create_window_record,
    find_window_mut, window_client_size,
};
use crate::{FontDialogPolicy, HandlerContext, OuterReturn, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

// ── ChooseFontW (commdlg.h / wingdi.h field offsets, Win64) ───────────────

/// `CHOOSEFONTW.hwndOwner` — the dialog's owner window.
const CF_HWND_OWNER: u64 = 0x08;
/// `CHOOSEFONTW.lpLogFont` — pointer to the `LOGFONTW` written back.
const CF_LP_LOG_FONT: u64 = 0x18;
/// `CHOOSEFONTW.iPointSize` — returned size in tenths of points.
const CF_IPOINT_SIZE: u64 = 0x20;
/// `CHOOSEFONTW.Flags` — `CF_*` bits (commdlg.h).
const CF_FLAGS: u64 = 0x24;
/// `CHOOSEFONTW.rgbColors` — returned text color.
const CF_RGB_COLORS: u64 = 0x28;

/// `LOGFONTW.lfHeight` (negative = character height in px).
const LF_HEIGHT: u64 = 0x00;
/// `LOGFONTW.lfItalic` .. `lfCharSet` — one byte each, packed into a u32:
/// italic (bit 0), underline (byte 1), strikeout (byte 2), charset (byte 3).
/// `lfWeight` (offset 0x10) and the other untouched fields are preserved by
/// the write-back (it only overwrites `lfHeight`, this word, and `lfFaceName`).
const LF_ITALIC_UNDERLINE_STRIKE_CHARSET: u64 = 0x14;
/// `LOGFONTW.lfFaceName` — `WCHAR[32]` (`LF_FACESIZE`).
const LF_FACE_NAME: u64 = 0x1C;

/// `CF_SCREENFONTS` (commdlg.h) — the dialog serves screen fonts.
const CF_SCREEN_FONTS: u32 = 0x1;

/// Control ids inside the font dialog (must differ from `IDOK`/`IDCANCEL`).
///
/// The two effects ids are the sentinel results the shared dialog-proc stub
/// (`encode_file_dialog_proc`) passes to `EndDialog`; the `EndDialog` handler
/// turns them into checkbox toggles instead of closing the dialog. They are
/// `pub` because the wie-runtime stub encoder embeds them in machine code.
pub const FONT_DLG_STRIKEOUT_ID: u64 = 1302;
pub const FONT_DLG_UNDERLINE_ID: u64 = 1303;
const FONT_DLG_FAMILY_LIST_ID: u64 = 1300;
const FONT_DLG_SIZE_LIST_ID: u64 = 1301;

/// Font-dialog window size (pixels).
///
/// The height leaves the Strikeout/Underline effects row (y=224, 20 px tall)
/// fully INSIDE the dialog with a bottom margin — the pre-fix 230 clipped
/// the buttons' bottom 14 px off the dialog (the reported "Strikeout and
/// Underline are outside the dialog" bug). 260 matches the classic Windows
/// font dialog's ~320×260 proportions.
const FONT_DLG_CX: i32 = 340;
const FONT_DLG_CY: i32 = 260;
/// Handles `comdlg32.dll!ChooseFontW`.
///
/// Under [`FontDialogPolicy::Cancel`] (the default for headless runs) returns
/// FALSE like a user canceling. Under [`FontDialogPolicy::Interactive`] builds
/// the host font dialog and runs its in-guest modal loop (see
/// [`open_host_font_dialog`]).
pub fn handle_choose_font_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cf_va = read_arg(engine, ArgReg::Rcx, "ChooseFontW")?;

    if cf_va == 0 {
        state_comm_dlg_none(state);
        return ctx.finish(0);
    }

    // Clone the policy to avoid borrowing window_state() across the match.
    let policy = state.window_state().font_dialog_policy.clone();
    match policy {
        FontDialogPolicy::Cancel => {
            state_comm_dlg_none(state);
            tracing::debug!("ChooseFontW cancelled by policy");
            ctx.finish(0)
        }
        FontDialogPolicy::Interactive => open_host_font_dialog(ctx, cf_va),
    }
}
// ── ChooseFontW interactive dialog ─────────────────────────────────────────

/// Whether `hwnd` is the in-flight modal font dialog.
///
/// The `EndDialog` handler routes on this: the font dialog's write-back
/// (`complete_font_dialog`) handles the shared proc stub's sentinel results
/// (effects toggles that keep the dialog open) in addition to OK/Cancel.
#[must_use]
pub(crate) fn is_font_dialog_window(state: &WinApiState, hwnd: u64) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.font_dialog
            .as_ref()
            .is_some_and(|session| session.dialog_hwnd == hwnd)
    })
}

/// Windows `MulDiv(a, b, c)` (wingdi.h): `(a * b + c / 2) / c`, signed
/// rounding toward zero on the half. Used for the point ↔ pixel height
/// conversion (the same formula RNotepad's `HeightFromPointSize` uses).
fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    let product = a * b;
    if product >= 0 {
        (product + c / 2) / c
    } else {
        (product - c / 2) / c
    }
}

/// Point size (tenths of points) for the initial `LOGFONTW.lfHeight`.
///
/// `lfHeight < 0` is a character height in pixels; at 96 DPI one point is
/// 4/3 px, so `tenths = MulDiv(720, |height|, 96)`. `0` (the engine's default
/// 16 px) maps to 12 points. Rounded to a whole point so the size LISTBOX
/// (integer points 8..72) can seed its selection.
fn point_size_tenths_from_lf_height(lf_height: i32) -> i32 {
    let px = if lf_height == 0 {
        16
    } else {
        lf_height.unsigned_abs().max(1)
    };
    let tenths = mul_div(720, i64::from(px), 96).max(1);
    // Round to the nearest whole point (the list only offers integers).
    i32::try_from((tenths + 5) / 10 * 10)
        .unwrap_or(120)
        .clamp(80, 720)
}

/// `LOGFONTW.lfHeight` for a point size in tenths: `-MulDiv(t, 96, 720)`.
fn lf_height_from_point_size_tenths(point_tenths: i32) -> i32 {
    let negative = -mul_div(i64::from(point_tenths), 96, 720);
    i32::try_from(negative).unwrap_or(-16)
}

/// Family names for the dialog's LISTBOX, from the host font database.
fn dialog_family_names() -> Vec<String> {
    let mut names = crate::gdi32::system_family_names();
    // Keep the list predictable even on a font-less host.
    if names.is_empty() {
        names.push("System".to_owned());
    }
    names
}

/// Point-size items (whole points 8..72) for the dialog's size LISTBOX.
fn dialog_point_sizes() -> Vec<String> {
    (8..=72).map(|point| point.to_string()).collect()
}

/// `FontDialogPolicy::Interactive`: build the host font dialog (a
/// "FontDialog"-class window with a family LISTBOX, a size LISTBOX, the
/// Strikeout/Underline effects buttons, and OK/Cancel) and ask the runtime to
/// run the file dialog's in-guest modal loop.
///
/// The dialog window carries the planted file-dialog proc stub as its
/// `dialog_proc`, so `WM_COMMAND(IDOK/IDCANCEL)` / `WM_CLOSE` bridge into the
/// stub, which calls `EndDialog`. The extended `EndDialog` handler performs
/// the `CHOOSEFONTW`/`LOGFONTW` write-back (see [`complete_font_dialog`]) and
/// posts the `WM_QUIT` the loop exits on. The effects buttons close through
/// the same stub with sentinel results, which the handler turns into toggles
/// (the dialog stays open).
fn open_host_font_dialog(ctx: &mut HandlerContext<'_>, cf_va: u64) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let log_font_va = read_u64(
        engine,
        checked_address(cf_va, CF_LP_LOG_FONT, "CHOOSEFONTW.lpLogFont"),
    )
    .context("failed to read lpLogFont for ChooseFontW")?;
    let initial_point_tenths = read_u32(
        engine,
        checked_address(cf_va, CF_IPOINT_SIZE, "CHOOSEFONTW.iPointSize"),
    )
    .context("failed to read iPointSize for ChooseFontW")?;
    let flags = read_u32(
        engine,
        checked_address(cf_va, CF_FLAGS, "CHOOSEFONTW.Flags"),
    )
    .context("failed to read Flags for ChooseFontW")?;
    let rgb_colors = read_u32(
        engine,
        checked_address(cf_va, CF_RGB_COLORS, "CHOOSEFONTW.rgbColors"),
    )
    .context("failed to read rgbColors for ChooseFontW")?;

    // Without the planted loop/proc bodies (headless/trace sessions) or with a
    // font dialog already open, fall back to Cancel: a guest must never hang.
    let loop_va = state.window_state().file_dialog_loop_va;
    let proc_va = state.window_state().file_dialog_proc_va;
    if log_font_va == 0
        || loop_va == 0
        || proc_va == 0
        || state.window_state().font_dialog.is_some()
    {
        state_comm_dlg_none(state);
        tracing::warn!("ChooseFontW interactive dialog unavailable; cancelling");
        return ctx.finish(0);
    }

    // Seed the dialog from the guest's initial LOGFONTW (CF_INITTOLOGFONTSTRUCT
    // semantics — RNotepad initializes `lf` from its stored font). The guest
    // stack frame holding `lf` stays live for the whole modal loop, so the
    // write-back re-reads it rather than caching the bytes here.
    let initial_face = read_utf16_lossy(
        engine,
        checked_address(log_font_va, LF_FACE_NAME, "LOGFONTW.lfFaceName"),
        32,
    )
    .context("failed to read ChooseFontW lfFaceName")?;
    let lf_height = read_i32(
        engine,
        checked_address(log_font_va, LF_HEIGHT, "LOGFONTW.lfHeight"),
    )
    .context("failed to read ChooseFontW lfHeight")?;
    let effects_word = read_u32(
        engine,
        checked_address(
            log_font_va,
            LF_ITALIC_UNDERLINE_STRIKE_CHARSET,
            "LOGFONTW effects",
        ),
    )
    .context("failed to read ChooseFontW underline/strikeout")?;

    let family_names = dialog_family_names();
    let point_sizes = dialog_point_sizes();
    let seed_family_index = family_names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(&initial_face))
        .unwrap_or(0);
    let seed_size_index = if initial_point_tenths != 0 {
        let point = i32::try_from(initial_point_tenths / 10).unwrap_or(10);
        point.clamp(8, 72).saturating_sub(8).clamp(
            0,
            i32::try_from(point_sizes.len().saturating_sub(1)).unwrap_or(0),
        )
    } else {
        // lfHeight 0 → the engine's 16 px default → 12 points.
        let point = point_size_tenths_from_lf_height(lf_height) / 10;
        point.saturating_sub(8).clamp(0, 64)
    };
    let seed_family = family_names
        .get(seed_family_index)
        .cloned()
        .unwrap_or_default();
    let seed_point_size = point_sizes
        .get(usize::try_from(seed_size_index).unwrap_or(0))
        .and_then(|text| text.parse::<i32>().ok())
        .unwrap_or(10)
        .saturating_mul(10);

    let owner_raw = read_u64(
        engine,
        checked_address(cf_va, CF_HWND_OWNER, "CHOOSEFONTW.hwndOwner"),
    )
    .context("failed to read hwndOwner for ChooseFontW")?;
    let parent_handle = resolve_dialog_owner(state, owner_raw);
    let (owner_w, owner_h) = window_client_size(state, parent_handle);
    let (dialog_x, dialog_y) =
        if parent_handle != 0 && owner_w >= FONT_DLG_CX && owner_h >= FONT_DLG_CY {
            (
                owner_w.saturating_sub(FONT_DLG_CX).saturating_div(2),
                owner_h.saturating_sub(FONT_DLG_CY).saturating_div(2),
            )
        } else {
            (0, 0)
        };

    let (dialog_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("FontDialog".to_owned()),
            title: "Font".to_owned(),
            style: WS_VISIBLE | WS_CLIPCHILDREN,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x: dialog_x,
            y: dialog_y,
            width: FONT_DLG_CX,
            height: FONT_DLG_CY,
        },
        false,
    )?;
    if dialog_hwnd == 0 {
        state_comm_dlg_none(state);
        return ctx.finish(0);
    }
    if let Some(window) = find_window_mut(state, dialog_hwnd) {
        window.dialog_proc = proc_va;
        window.dialog_unicode = false;
        window.client_rect = (0, 0, FONT_DLG_CX, FONT_DLG_CY);
    }

    // Row 0: "Font:" label + family LISTBOX (from the host font database).
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Font:".to_owned(),
        0,
        0,
        8,
        8,
        60,
        18,
    )?;
    let family_list_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0083), // LISTBOX
        String::new(),
        0,
        FONT_DLG_FAMILY_LIST_ID,
        72,
        8,
        168,
        140,
    )?;
    // Row 1: "Size:" label + size LISTBOX (whole points 8..72).
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Size:".to_owned(),
        0,
        0,
        8,
        156,
        60,
        18,
    )?;
    let size_list_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0083), // LISTBOX
        String::new(),
        0,
        FONT_DLG_SIZE_LIST_ID,
        72,
        156,
        60,
        60,
    )?;

    // Effects checkboxes render as push buttons whose caption shows the state
    // (the find-dialog precedent); the checked state lives on the session and
    // is mirrored into the LOGFONTW on OK. The shared dialog-proc stub maps
    // these ids to sentinel EndDialog results the handler turns into toggles.
    let strikeout_checked = effects_word >> 16 & 0xFF != 0;
    let underline_checked = effects_word >> 8 & 0xFF != 0;
    let strikeout_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if strikeout_checked {
            "[x] Strikeout".to_owned()
        } else {
            "[ ] Strikeout".to_owned()
        },
        0,
        FONT_DLG_STRIKEOUT_ID,
        16,
        224,
        100,
        20,
    )?;
    let underline_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if underline_checked {
            "[x] Underline".to_owned()
        } else {
            "[ ] Underline".to_owned()
        },
        0,
        FONT_DLG_UNDERLINE_ID,
        120,
        224,
        100,
        20,
    )?;

    // OK (the dialog's Enter default) and Cancel.
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "OK".to_owned(),
        BS_DEFPUSHBUTTON,
        IDOK,
        FONT_DLG_CX.saturating_sub(176),
        200,
        80,
        24,
    )?;
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Cancel".to_owned(),
        0,
        IDCANCEL,
        FONT_DLG_CX.saturating_sub(88),
        200,
        80,
        24,
    )?;

    // Seed the LISTBOX items + initial selections.
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(family_list_hwnd))
        .or_insert_with(|| ControlClassKind::ListBox.new_state());
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(size_list_hwnd))
        .or_insert_with(|| ControlClassKind::ListBox.new_state());
    if let ControlState::ListBox {
        items, sel_index, ..
    } = state
        .window_state()
        .control_states
        .get_mut(&Hwnd::from(family_list_hwnd))
        .context("family listbox state")?
    {
        *items = family_names;
        *sel_index = i32::try_from(seed_family_index).unwrap_or(0);
    }
    if let ControlState::ListBox {
        items, sel_index, ..
    } = state
        .window_state()
        .control_states
        .get_mut(&Hwnd::from(size_list_hwnd))
        .context("size listbox state")?
    {
        *items = point_sizes;
        *sel_index = seed_size_index;
    }
    // Bring the seeded selections into view: the guest's current family may
    // sit far down the host database, and the size seed at 12 pt is near the
    // top — the viewport opens showing the selected rows (Windows behavior).
    let _scrolled_family =
        crate::user32::controls::listbox_scroll_selection_into_view(state, family_list_hwnd);
    let _scrolled_size =
        crate::user32::controls::listbox_scroll_selection_into_view(state, size_list_hwnd);

    state.window_state().font_dialog = Some(FontDialogSession {
        dialog_hwnd,
        cf_ptr: cf_va,
        log_font_ptr: log_font_va,
        rgb_colors,
        flags,
        family_list_hwnd,
        size_list_hwnd,
        strikeout_hwnd,
        underline_hwnd,
        strikeout_checked,
        underline_checked,
        selected_family: seed_family,
        selected_point_size: seed_point_size,
    });

    // The dialog is modal: an empty GetMessage must yield, and the dialog
    // takes activation. The family LISTBOX gets the initial keyboard focus
    // (host-side WM_SETFOCUS).
    let (frame, _signal) = ModalFrame::activate(
        state,
        engine,
        dialog_hwnd,
        Some(family_list_hwnd),
        &[
            dialog_hwnd,
            family_list_hwnd,
            size_list_hwnd,
            strikeout_hwnd,
            underline_hwnd,
        ],
    )?;
    // Store the frame so EndDialog's shared teardown can finish this session.
    state
        .window_state()
        .modal_frames
        .insert(Hwnd::from(dialog_hwnd), frame);

    // A freshly created dialog is a visible change: bump the content revision
    // so the idle reconcile republishes its first painted frame.
    crate::present::PresentState::request_paint(state, dialog_hwnd);

    tracing::info!(
        target: "wiegui",
        hwnd = dialog_hwnd,
        parent = parent_handle,
        "font dialog opened"
    );

    // Run the planted modal loop in-guest; its return value (the dialog
    // result slot: 1 on OK, 0 on cancel) becomes the ChooseFont return.
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: loop_va,
            window_handle: dialog_hwnd,
            message: 0,
            word_parameter: 0,
            long_parameter: 0,
            unicode: false,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}

/// `EndDialog` write-back for a closing font dialog.
///
/// Three result shapes (the shared dialog-proc stub passes the control id):
/// - `FONT_DLG_STRIKEOUT_ID` / `FONT_DLG_UNDERLINE_ID` — an effects toggle:
///   flip the session's checkbox state + caption, repaint, and return `None`
///   so the modal loop keeps running (the dialog stays open).
/// - `1` (OK) — write the selection into the guest `LOGFONTW` (via
///   `lpLogFont`) and the `CHOOSEFONTW` fields, return `Some(1)`.
/// - `0` (Cancel / WM_CLOSE) — no write-back, return `Some(0)`.
///
/// The `LOGFONTW` is re-read from guest memory at write-back (the guest's
/// stack frame holds it live across the modal loop), so untouched fields —
/// `lfWeight`, `lfCharSet`, `lfItalic`, `lfPitchAndFamily`, … — are preserved
/// exactly and only the dialog-owned fields (`lfHeight`, `lfUnderline`,
/// `lfStrikeOut`, `lfFaceName`) change.
pub(crate) fn complete_font_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    dialog_hwnd: u64,
    result: u64,
) -> Result<Option<u64>> {
    let Some(session) = state.window_state().font_dialog.clone() else {
        return Ok(Some(result));
    };
    if session.dialog_hwnd != dialog_hwnd {
        return Ok(Some(result));
    }

    // Effects toggle: flip the session checkbox state + caption, keep open.
    if result == FONT_DLG_STRIKEOUT_ID || result == FONT_DLG_UNDERLINE_ID {
        let (checked, checkbox_hwnd, label) = {
            let font_dialog = state
                .window_state()
                .font_dialog
                .as_mut()
                .context("font-dialog session vanished")?;
            if result == FONT_DLG_STRIKEOUT_ID {
                font_dialog.strikeout_checked = !session.strikeout_checked;
                (
                    font_dialog.strikeout_checked,
                    session.strikeout_hwnd,
                    "Strikeout",
                )
            } else {
                font_dialog.underline_checked = !session.underline_checked;
                (
                    font_dialog.underline_checked,
                    session.underline_hwnd,
                    "Underline",
                )
            }
        };
        tracing::debug!(target: "wiegui", label, checked, "font dialog effects toggled");
        if let Some(window) = find_window_mut(state, checkbox_hwnd) {
            window.control_text = format!("[{}] {label}", if checked { "x" } else { " " });
            window.invalidated = true;
        }
        return Ok(None);
    }

    // Read the current selection from the LISTBOX controls.
    let (family, point_size_tenths) = {
        let window_state = state.window_state();
        let family = window_state
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .and_then(|control_state| match control_state {
                ControlState::ListBox {
                    items, sel_index, ..
                } => {
                    let index = usize::try_from(*sel_index).ok()?;
                    items.get(index).cloned()
                }
                _ => None,
            })
            .unwrap_or_else(|| session.selected_family.clone());
        let point = window_state
            .control_states
            .get(&Hwnd::from(session.size_list_hwnd))
            .and_then(|control_state| match control_state {
                ControlState::ListBox {
                    items, sel_index, ..
                } => {
                    let index = usize::try_from(*sel_index).ok()?;
                    items.get(index).and_then(|text| text.parse::<i32>().ok())
                }
                _ => None,
            })
            .unwrap_or(session.selected_point_size / 10);
        (family, point.saturating_mul(10).max(1))
    };
    let strikeout_checked = state
        .window_state()
        .font_dialog
        .as_ref()
        .map_or(session.strikeout_checked, |s| s.strikeout_checked);
    let underline_checked = state
        .window_state()
        .font_dialog
        .as_ref()
        .map_or(session.underline_checked, |s| s.underline_checked);

    if result == 0 {
        state.window_state().font_dialog = None;
        return Ok(Some(0));
    }

    // OK: write the selection into the guest LOGFONTW, preserving every field
    // the dialog does not own (weight, charset, italic, precision, …). The
    // typed views are READ-MODIFY-WRITE: `with_typed_write` zero-fills the
    // view first, so the original struct is captured through a read view and
    // restored before the dialog-owned fields change — the untouched fields
    // and the explicit pads carry the guest's original bytes, byte-identical
    // to the old per-field writes.
    let original_log_font =
        with_typed_read::<LogFontW, _, _>(engine, session.log_font_ptr, |log_font| Ok(*log_font))
            .context("failed to read LOGFONTW on font-dialog accept")?;
    let mut choose_font =
        with_typed_read::<ChooseFontW, _, _>(engine, session.cf_ptr, |cf| Ok(*cf))
            .context("failed to read CHOOSEFONTW on font-dialog accept")?;

    let lf_height = lf_height_from_point_size_tenths(point_size_tenths);
    // The dialog owns `lfPitchAndFamily` (real Windows' ChooseFont writes the
    // SELECTED font's pitch). The guest LOGFONT may carry the previous font's
    // FIXED_PITCH (notepad's default face is Lucida Console); a proportional
    // pick must clear it, or the guest's CreateFontIndirectW resolves the
    // chosen proportional family to monospace via the pitch-substitution rule
    // (the live "size applies, family does not" bug). The family nibble
    // (FF_*) is preserved.
    let fixed_pitch = crate::gdi32::family_is_monospaced(&family);
    let mut pitch_and_family = original_log_font.pitch_and_family;
    if fixed_pitch {
        pitch_and_family |= 0x01;
    } else {
        pitch_and_family &= !0x01;
    }
    with_typed_write::<LogFontW, _, _>(engine, session.log_font_ptr, |log_font| {
        *log_font = original_log_font;
        log_font.height = lf_height;
        log_font.underline = u8::from(underline_checked);
        log_font.strike_out = u8::from(strikeout_checked);
        log_font.pitch_and_family = pitch_and_family;
        // lfFaceName is a NUL-terminated WCHAR[LF_FACESIZE=32]: at most 31
        // units (mirrors write_utf16_c_string's truncation).
        let mut face_name = [0_u16; 32];
        for (slot, unit) in face_name.iter_mut().zip(family.encode_utf16().take(31)) {
            *slot = unit;
        }
        log_font.face_name = face_name;
        Ok(())
    })
    .context("failed to write LOGFONTW on font-dialog accept")?;

    choose_font.i_point_size = point_size_tenths;
    choose_font.flags = session.flags | CF_SCREEN_FONTS;
    choose_font.rgb_colors = session.rgb_colors;
    with_typed_write::<ChooseFontW, _, _>(engine, session.cf_ptr, |cf| {
        *cf = choose_font;
        Ok(())
    })
    .context("failed to write CHOOSEFONTW on font-dialog accept")?;

    state.window_state().font_dialog = None;
    tracing::info!(
        target: "wiegui",
        %family,
        point_tenths = point_size_tenths,
        strikeout_checked,
        underline_checked,
        "font dialog accepted"
    );
    Ok(Some(1))
}

#[cfg(test)]
mod tests {
    use super::{dialog_family_names, handle_choose_font_w};
    use crate::comdlg32::test_support::{
        create_owner_window, read_guest_u32_at, read_guest_utf16, test_engine, test_environment,
        test_state, utf16_bytes, write_regs,
    };
    use crate::handles::Hwnd;
    use crate::user32::controls::ControlState;
    use crate::user32::dialog::handle_end_dialog;
    use crate::user32::{IDOK, find_window};
    use crate::{FontDialogPolicy, HandlerContext, WinApiHandlerResult, WinApiState};
    use wie_cpu::{CpuEngine, IcedCpu};

    // ── ChooseFontW (L6) ──────────────────────────────────────────────────

    /// Write a `LOGFONTW` into guest memory at `ptr`.
    fn write_logfont(engine: &mut IcedCpu, ptr: u64, face: &str, charset: u8) {
        engine.mem_write(ptr, &0_i32.to_le_bytes()).ok(); // lfHeight (0 → 12 pt seed)
        engine.mem_write(ptr + 0x10, &400_i32.to_le_bytes()).ok(); // lfWeight = FW_NORMAL
        engine
            .mem_write(ptr + 0x14, &(u32::from(charset) << 24).to_le_bytes())
            .ok(); // lfItalic/Underline/StrikeOut = 0, lfCharSet = charset
        engine.mem_write(ptr + 0x1C, &utf16_bytes(face)).ok(); // lfFaceName
    }

    /// Write a `CHOOSEFONTW` into guest memory at `cf_va` (Win64 layout).
    fn write_choosefont(engine: &mut IcedCpu, cf_va: u64, logfont_va: u64, flags: u32, rgb: u32) {
        engine.mem_write(cf_va, &0x60_u32.to_le_bytes()).ok(); // lStructSize
        engine.mem_write(cf_va + 8, &0_u64.to_le_bytes()).ok(); // hwndOwner
        engine
            .mem_write(cf_va + 0x18, &logfont_va.to_le_bytes())
            .ok(); // lpLogFont
        engine.mem_write(cf_va + 0x20, &0_u32.to_le_bytes()).ok(); // iPointSize
        engine.mem_write(cf_va + 0x24, &flags.to_le_bytes()).ok(); // Flags
        engine.mem_write(cf_va + 0x28, &rgb.to_le_bytes()).ok(); // rgbColors
    }

    /// Drive `ChooseFontW` with a scripted policy; returns the result value.
    fn dispatch_choose_font(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        policy: FontDialogPolicy,
    ) -> anyhow::Result<WinApiHandlerResult> {
        state.window_state().font_dialog_policy = policy;
        write_regs(engine, 0x5000, 0, 0, 0);
        handle_choose_font_w(&mut HandlerContext::new(engine, test_environment(), state))
    }

    /// A font-dialog test scaffold: `CHOOSEFONTW` at 0x5000, `LOGFONTW` at
    /// 0x6000, loop/proc stubs wired, Interactive policy.
    fn font_dialog_scaffold(engine: &mut IcedCpu, state: &mut WinApiState, face: &str, flags: u32) {
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        write_logfont(engine, 0x6000, face, 1); // DEFAULT_CHARSET
        write_choosefont(engine, 0x5000, 0x6000, flags, 0x00_30_50);
        dispatch_choose_font(engine, state, FontDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
    }

    #[test]
    fn choose_font_cancel_policy_returns_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_logfont(&mut engine, 0x6000, "Arial", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0x1 | 0x40 | 0x100, 0);

        let result = dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Cancel)
            .expect("cancel must succeed");
        assert_eq!(result.return_value, 0);
        assert!(state.window_state().font_dialog.is_none());
        // The LOGFONTW is untouched by a cancel.
        assert_eq!(read_guest_utf16(&mut engine, 0x601C, 32), "Arial");
    }

    #[test]
    fn choose_font_without_loop_machinery_falls_back_to_cancel() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_logfont(&mut engine, 0x6000, "Arial", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0, 0);

        let result = dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Interactive)
            .expect("fallback must succeed");
        assert_eq!(result.return_value, 0, "no host dialog → cancel");
        assert!(state.window_state().font_dialog.is_none());
    }

    #[test]
    fn choose_font_interactive_builds_dialog_and_requests_loop() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);

        let dialog_hwnd = state
            .window_state()
            .font_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The dialog + its controls exist; the dialog carries the proc stub.
        // dialog + "Font:" label + family list + "Size:" label + size list +
        // Strikeout + Underline + OK + Cancel = 9 windows.
        let windows = &state.window_state().windows;
        assert_eq!(windows.len(), 9, "font dialog + 8 controls");
        let dialog = find_window(&mut state, dialog_hwnd).expect("dialog window");
        assert_eq!(
            dialog.dialog_proc, 0x7000_0040_B100,
            "dialog proc = file-dialog stub"
        );
        // Modal: depth up, dialog active, family list focused.
        assert_eq!(state.lock_message_queue().dialog_depth, 1);
        assert_eq!(
            state.window_state().active_window_handle.as_u64(),
            dialog_hwnd
        );
        assert_eq!(
            state.window_state().focus_window_handle.as_u64(),
            session.family_list_hwnd
        );
        // The family list carries the host database families with a selection.
        let ControlState::ListBox {
            items, sel_index, ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .expect("family listbox state")
        else {
            panic!("family list is a listbox");
        };
        assert!(!items.is_empty(), "family list seeded from the host db");
        assert_eq!(*sel_index, 0, "default selection is the first family");
        // The size list offers whole points 8..72 with the 12 pt seed.
        let ControlState::ListBox {
            items: sizes,
            sel_index: size_sel,
            ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.size_list_hwnd))
            .expect("size listbox state")
        else {
            panic!("size list is a listbox");
        };
        assert_eq!(sizes.len(), 65, "8..72 inclusive");
        assert_eq!(*size_sel, 4, "lfHeight 0 → 12 pt seed");
    }

    /// Paint the whole font-dialog subtree exactly like the pump's repaint
    /// cycle: the dialog face first (WM_PAINT → paint_dialog), then each
    /// visible control's WM_PAINT via the control dispatch.
    fn paint_font_dialog_subtree(engine: &mut IcedCpu, state: &mut WinApiState, dialog_hwnd: u64) {
        crate::user32::dialog::paint_dialog(state, dialog_hwnd);
        let children: Vec<u64> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(dialog_hwnd))
            .map(|w| w.handle.as_u64())
            .collect();
        for hwnd in children {
            crate::user32::dispatch_control_proc(
                engine,
                state,
                hwnd,
                crate::user32::WinMsg::WM_PAINT.as_u32(),
                0,
                0,
            )
            .expect("control WM_PAINT");
        }
    }

    #[test]
    fn font_dialog_survives_control_click_in_owner_frame() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        // Open the font dialog parented to the owner (hwndOwner → owner).
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        write_logfont(&mut engine, 0x6000, "", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0x1 | 0x40, 0x00_30_50);
        engine.mem_write(0x5008, &owner_hwnd.to_le_bytes()).ok();
        dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();
        let dialog_hwnd = session.dialog_hwnd;

        // Open cycle: paint the face + every control, then drain.
        paint_font_dialog_subtree(&mut engine, &mut state, dialog_hwnd);
        state.present().drain_pending_publishes();

        let owner = Hwnd::from(owner_hwnd);
        let sample = |state: &mut WinApiState| -> Option<u32> {
            let frame = state.present().published.get(&owner)?.clone();
            let idx = 190_usize * frame.stride as usize + 235_usize;
            frame.pixels.get(idx).copied()
        };
        // The dialog face (BTNFACE) must be present in the owner frame.
        assert_eq!(
            sample(&mut state),
            Some(0x00F0_F0F0),
            "dialog face must appear in the owner frame after the open cycle"
        );

        // Click the family listbox (dialog-relative (72,8,168,140); click at
        // child-relative (20,40) → item row 2).
        let lparam = u64::from(40_u32 << 16 | 20_u32);
        let click = crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            session.family_list_hwnd,
            crate::user32::WinMsg::WM_LBUTTONDOWN.as_u32(),
            1,
            lparam,
        );
        assert!(
            click.is_err(),
            "the selection change notifies the dialog proc"
        );

        // The click invalidated the listbox: repaint it (the next cycle).
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            session.family_list_hwnd,
            crate::user32::WinMsg::WM_PAINT.as_u32(),
            0,
            0,
        )
        .expect("listbox WM_PAINT");
        state.present().drain_pending_publishes();

        assert_eq!(
            sample(&mut state),
            Some(0x00F0_F0F0),
            "dialog face must SURVIVE a click on a listbox control"
        );
        // The listbox area itself is white (COLOR_WINDOW) and the clicked row
        // is highlighted (COLOR_HIGHLIGHT) somewhere in the listbox rect
        // (302,193)-(470,333) in the owner — the selection followed the click.
        let frame = state
            .present()
            .published
            .get(&owner)
            .expect("owner frame after click")
            .clone();
        let highlight = (193..333).fold(0_u32, |acc, y| {
            acc + (302..470).fold(0_u32, |acc, x| {
                let idx = y as usize * frame.width as usize + x as usize;
                acc + u32::from(frame.pixels.get(idx).copied() == Some(0x0000_78D7))
            })
        });
        assert!(
            highlight > 100,
            "the clicked listbox row must be highlighted (COLOR_HIGHLIGHT); found {highlight} px"
        );
    }

    #[test]
    fn font_dialog_controls_fit_inside_the_dialog_bounds() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();
        let dialog = find_window(&mut state, session.dialog_hwnd).expect("dialog window");
        let (dialog_w, dialog_h) = (dialog.width, dialog.height);
        assert_eq!(
            (dialog_w, dialog_h),
            (340, 260),
            "font dialog is ~320x260-ish"
        );

        let children: Vec<(i32, i32, i32, i32, String)> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(session.dialog_hwnd))
            .map(|w| (w.x, w.y, w.width, w.height, w.class_name.clone()))
            .collect();
        assert_eq!(children.len(), 8, "label + list + label + list + 4 buttons");
        for (x, y, w, h, class) in children {
            assert!(x >= 0 && y >= 0, "{class} sits at a negative position");
            assert!(
                x.saturating_add(w) <= dialog_w,
                "{class} overflows the dialog's right edge (x={x} w={w} dialog_w={dialog_w})"
            );
            assert!(
                y.saturating_add(h) <= dialog_h,
                "{class} overflows the dialog's bottom edge (y={y} h={h} \
                 dialog_h={dialog_h}) — the Strikeout/Underline buttons were \
                 outside the dialog before the height fix"
            );
        }
    }

    #[test]
    fn font_dialog_family_list_scrolls_via_wheel_and_arrows() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The family list holds the whole host database — far more rows than
        // the 140 px tall listbox can show, so the wheel must scroll it.
        let family = session.family_list_hwnd;
        let list_state = |state: &mut WinApiState| {
            let ControlState::ListBox {
                items,
                sel_index,
                first_visible,
                ..
            } = state
                .window_state()
                .control_states
                .get(&Hwnd::from(family))
                .expect("family listbox state")
            else {
                panic!("family list is a listbox");
            };
            (items.len(), *sel_index, *first_visible)
        };
        let (count, sel, first) = list_state(&mut state);
        assert!(count > 10, "the host db must overflow the listbox viewport");
        assert_eq!(sel, 0, "default selection is the first family");
        // The seeded selection (index 0) is already visible — no initial scroll.
        assert_eq!(first, 0);

        // Wheel down over the listbox: the viewport scrolls 3 rows.
        let wheel_down = u64::from(u16::MAX - 119) << 16; // delta = -120
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            family,
            crate::user32::WinMsg::WM_MOUSEWHEEL.as_u32(),
            wheel_down,
            0,
        )
        .expect("wheel scrolls the family list");
        let (_, _, first) = list_state(&mut state);
        assert_eq!(first, 3, "one wheel notch scrolls 3 rows");

        // Arrow keys move the selection and keep it visible (focused).
        let keydown = crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            family,
            crate::user32::WinMsg::WM_KEYDOWN.as_u32(),
            0x28, // VK_DOWN
            0,
        );
        assert!(
            keydown.is_err(),
            "a selection change notifies the dialog proc"
        );
        let (_, sel, first) = list_state(&mut state);
        assert_eq!(sel, 1, "VK_DOWN moves the selection one row");
        assert!(
            sel >= i32::try_from(first).unwrap_or(0),
            "the selection stays visible after the key move"
        );
    }

    #[test]
    fn end_dialog_font_writes_logfont_and_choosefont_back() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The seeded family is whatever the host db lists first; the write-back
        // must mirror exactly that (plus the seeded 12 pt size).
        let ControlState::ListBox {
            items, sel_index, ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .expect("family listbox state")
        else {
            panic!("family list is a listbox");
        };
        let family = items
            .get(usize::try_from(*sel_index).unwrap_or(0))
            .unwrap()
            .clone();

        // OK: EndDialog(1) → the selection lands in the LOGFONTW + CHOOSEFONTW.
        write_regs(&mut engine, session.dialog_hwnd, IDOK, 0, 0);
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1);

        // LOGFONTW: lfHeight = -MulDiv(120, 96, 720) = -16; face written;
        // underline/strikeout/charset preserved (0/0/1); weight untouched.
        let mut height = [0_u8; 4];
        engine.mem_read(0x6000, &mut height).ok();
        assert_eq!(i32::from_le_bytes(height), -16, "12 pt at 96 DPI → -16 px");
        assert_eq!(read_guest_utf16(&mut engine, 0x601C, 32), family);
        let mut effects = [0_u8; 4];
        engine.mem_read(0x6014, &mut effects).ok();
        assert_eq!(
            u32::from_le_bytes(effects),
            1 << 24,
            "charset preserved, effects off"
        );
        let mut weight = [0_u8; 4];
        engine.mem_read(0x6010, &mut weight).ok();
        assert_eq!(i32::from_le_bytes(weight), 400, "lfWeight preserved");

        // CHOOSEFONTW: iPointSize in tenths, Flags OR CF_SCREENFONTS, rgbColors.
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5020),
            120,
            "12 pt in tenths"
        );
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5024),
            0x1 | 0x40 | 0x1,
            "guest flags + CF_SCREENFONTS"
        );
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5028),
            0x00_30_50,
            "rgbColors preserved"
        );
        assert!(
            state.window_state().font_dialog.is_none(),
            "session cleared"
        );
    }

    #[test]
    fn end_dialog_font_effects_toggle_keeps_dialog_open() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // EndDialog(strikeout sentinel) → toggle, no close.
        write_regs(
            &mut engine,
            session.dialog_hwnd,
            super::FONT_DLG_STRIKEOUT_ID,
            0,
            0,
        );
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1, "EndDialog itself succeeds");
        assert!(
            state.window_state().font_dialog.is_some(),
            "toggle must not close the dialog"
        );
        assert!(
            state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .strikeout_checked
        );
        assert!(
            !state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .underline_checked
        );
        let strikeout = find_window(&mut state, session.strikeout_hwnd).expect("strikeout button");
        assert_eq!(strikeout.control_text, "[x] Strikeout");

        // The underline toggle flips its own checkbox.
        write_regs(
            &mut engine,
            session.dialog_hwnd,
            super::FONT_DLG_UNDERLINE_ID,
            0,
            0,
        );
        handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert!(
            state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .underline_checked
        );

        // OK now closes and writes the toggled effects into the LOGFONTW.
        write_regs(&mut engine, session.dialog_hwnd, IDOK, 0, 0);
        handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        let mut effects = [0_u8; 4];
        engine.mem_read(0x6014, &mut effects).ok();
        assert_eq!(
            u32::from_le_bytes(effects),
            (1 << 8) | (1 << 16) | (1 << 24),
            "underline + strikeout + charset written on OK"
        );
        assert!(state.window_state().font_dialog.is_none());
    }
    #[test]
    fn font_dialog_family_enumeration_has_system_families() {
        let families = dialog_family_names();
        assert!(
            !families.is_empty(),
            "host fontdb must name at least one family"
        );
        assert!(families.windows(2).all(|pair| pair[0] <= pair[1]), "sorted");
    }

    /// LIVE-symptom repro: the Font dialog's chosen FAMILY must round-trip —
    /// dialog pick → `LOGFONTW.lfFaceName` → `CreateFontIndirectW` → the font
    /// engine's resolution. The size path (lfHeight) is proven; the family
    /// path breaks because RNotepad's LOGFONT carries `FIXED_PITCH` (its
    /// default face is Lucida Console), and the dialog's write-back preserves
    /// `lfPitchAndFamily` — so a PROPORTIONAL pick is handed back to the guest
    /// with the OLD fixed-pitch flag, and the Windows pitch-substitution rule
    /// then substitutes monospace for the picked proportional family.
    #[test]
    fn font_dialog_picked_family_must_clear_fixed_pitch() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // Pick a PROPORTIONAL family from the list (the write-back must not
        // leave the default's FIXED_PITCH on a proportional pick).
        let picked = {
            let ws = state.window_state();
            let ControlState::ListBox {
                items, sel_index, ..
            } = ws
                .control_states
                .get_mut(&Hwnd::from(session.family_list_hwnd))
                .expect("family listbox state")
            else {
                panic!("family list is a listbox");
            };
            let index = items.len().min(2).saturating_sub(1);
            *sel_index = i32::try_from(index).unwrap_or(0);
            items[index].clone()
        };
        assert_ne!(
            picked,
            dialog_family_names()[0],
            "the repro must pick a non-default family"
        );

        // RNotepad's LOGFONT carries FIXED_PITCH|FF_MODERN (0x31) — its
        // default face is Lucida Console. The dialog's write-back must clear
        // the pitch bit for a proportional pick (real Windows' ChooseFont sets
        // lfPitchAndFamily to the selected font's pitch).
        engine.mem_write(0x601B, &[0x31]).ok();

        write_regs(&mut engine, session.dialog_hwnd, IDOK, 0, 0);
        handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");

        // The family was written into the guest LOGFONTW…
        let written = read_guest_utf16(&mut engine, 0x601C, 32);
        assert_eq!(
            written, picked,
            "the picked family must be written into LOGFONTW.lfFaceName"
        );
        // …and the FIXED_PITCH bit (0x01) must be cleared for the proportional
        // pick, or the guest's CreateFontIndirectW resolves it to monospace.
        let mut pitch = [0_u8; 1];
        engine.mem_read(0x601B, &mut pitch).ok();
        assert_eq!(
            pitch[0] & 0x01,
            0,
            "a proportional family pick must clear FIXED_PITCH in \
             lfPitchAndFamily (chosen '{picked}' kept the default's \
             fixed-pitch flag)"
        );

        // Full round-trip: the guest re-creates the font from the corrected
        // LOGFONT and applies it; the paint resolution must pick the CHOSEN
        // proportional family, not the monospace substitution.
        use crate::gdi32::{FontEngine, handle_create_font_indirect_w, window_font_resolution};
        write_regs(&mut engine, 0x6000, 0, 0, 0);
        let font = handle_create_font_indirect_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("CreateFontIndirectW succeeds")
        .return_value;
        let owner_hwnd = create_owner_window(&mut state);
        crate::user32::find_window_mut(&mut state, owner_hwnd)
            .expect("owner window")
            .font_handle = crate::handles::Hfont::from(font);
        let mut font_engine = FontEngine::default();
        let (key, _resolved) = window_font_resolution(&state, owner_hwnd, &mut font_engine)
            .expect("the picked font must resolve");
        assert_eq!(
            key.family,
            picked.to_ascii_lowercase(),
            "the chosen proportional family must survive dialog → LOGFONT → \
             CreateFontIndirect → resolution (got the default '{family}', \
             picked '{picked}')",
            family = key.family,
        );
    }
}
