//! Host-side WndProc for the built-in control classes (BUTTON, STATIC, EDIT,
//! LISTBOX, COMBOBOX).
//!
//! Built-in controls have no guest WndProc (`window_proc == 0`); the runtime
//! dispatches their messages here instead of returning the neutral zero.
//! Controls composite into their top-level ancestor's present surface at the
//! parent-relative offset resolved by
//! [`crate::gdi32::resolve_window_ancestor`]. After a control's `WM_PAINT`
//! the ancestor surface is published so the control is visible immediately —
//! the ancestor's own `WM_PAINT` BitBlt may never run again (modal dialogs).

use anyhow::Result;

use super::{
    BM_CLICK, BM_GETSTATE, BM_SETSTATE, BN_CLICKED, BS_DEFPUSHBUTTON, BST_FOCUS, BST_PUSHED,
    DLGC_BUTTON, DLGC_DEFPUSHBUTTON, DLGC_UNDEFPUSHBUTTON, DLGC_WANTCHARS, EM_GETSEL, EM_SETSEL,
    EN_CHANGE, GuestCallbackRequest, LB_ADDSTRING, LB_GETCOUNT, LB_GETCURSEL, LB_GETTEXT,
    LB_SETCURSEL, LBN_SELCHANGE, VK_DELETE, VK_END, VK_HOME, VK_LEFT, VK_RIGHT, VK_SHIFT, VK_SPACE,
    WM_CHAR, WM_COMMAND, WM_GETDLGCODE, WM_GETTEXT, WM_GETTEXTLENGTH, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_PAINT, WM_SETFOCUS, WM_SETTEXT,
    WinApiControlSignal, WinApiState, WindowClassIdentifier, find_window, find_window_mut, low_i32,
    read_guest_ansi_lossy, read_guest_utf16_lossy, write_guest_ansi_c_string, write_guest_u32,
    write_guest_utf16_c_string,
};
use crate::OuterReturn;
use crate::gdi32::ResolvedWindow;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::render_text_into_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};

/// `GetSysColor(COLOR_BTNFACE)` — the standard push-button face.
const COLOR_BTNFACE: u32 = 0x00F0_F0F0;
/// `GetSysColor(COLOR_BTNSHADOW)` — the standard button border gray.
const COLOR_BTNSHADOW: u32 = 0x00A0_A0A0;
/// `GetSysColor(COLOR_WINDOW)` — the standard EDIT / LISTBOX background.
const COLOR_WINDOW: u32 = 0x00FF_FFFF;
/// Slightly darker face while a button is pressed (matches the classic 3D
/// pressed look).
const COLOR_BTNFACE_PRESSED: u32 = 0x00D8_D8D8;
/// `GetSysColor(COLOR_HIGHLIGHT)` — the selection fill for EDIT / LISTBOX.
const COLOR_HIGHLIGHT: u32 = 0x0000_78D7;
/// `GetSysColor(COLOR_HIGHLIGHTTEXT)` — text drawn over the selection.
const COLOR_HIGHLIGHTTEXT: u32 = 0x00FF_FFFF;

/// A built-in USER32 control class, resolved at `CreateWindowEx` time.
///
/// Recognized by ordinal (`MAKEINTRESOURCE` class atom) and by the canonical
/// ANSI class names. The atom ordinals are the classic user32 registrations:
/// `BUTTON` 0x0080, `EDIT` 0x0081, `STATIC` 0x0082, `LISTBOX` 0x0083,
/// `COMBOBOX` 0x0085.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlClassKind {
    /// `BUTTON` (push buttons, check boxes, group boxes — push buttons only).
    Button,
    /// `STATIC` (labels; text-only painting).
    Static,
    /// `EDIT` (single/multi-line text input).
    Edit,
    /// `LISTBOX` (item list, no scrollbar yet).
    ListBox,
    /// `COMBOBOX` (edit+list; no dropdown yet).
    ComboBox,
}

impl ControlClassKind {
    /// Built-in class ordinals (user32's pre-registered atoms).
    const ORDINAL_BUTTON: u16 = 0x0080;
    const ORDINAL_EDIT: u16 = 0x0081;
    const ORDINAL_STATIC: u16 = 0x0082;
    const ORDINAL_LISTBOX: u16 = 0x0083;
    const ORDINAL_COMBOBOX: u16 = 0x0085;

    /// Resolve a `CreateWindowEx` class identifier to a built-in control
    /// class, by ordinal (`MAKEINTRESOURCE`) or by case-insensitive name.
    #[must_use]
    pub(crate) fn from_identifier(identifier: &WindowClassIdentifier) -> Option<Self> {
        match identifier {
            WindowClassIdentifier::Atom(atom) => match *atom {
                Self::ORDINAL_BUTTON => Some(Self::Button),
                Self::ORDINAL_EDIT => Some(Self::Edit),
                Self::ORDINAL_STATIC => Some(Self::Static),
                Self::ORDINAL_LISTBOX => Some(Self::ListBox),
                Self::ORDINAL_COMBOBOX => Some(Self::ComboBox),
                _ => None,
            },
            WindowClassIdentifier::Name(name) => {
                let upper = name.to_ascii_uppercase();
                match upper.as_str() {
                    "BUTTON" => Some(Self::Button),
                    "EDIT" => Some(Self::Edit),
                    "STATIC" => Some(Self::Static),
                    "LISTBOX" => Some(Self::ListBox),
                    "COMBOBOX" => Some(Self::ComboBox),
                    _ => None,
                }
            }
        }
    }
}

/// Per-window runtime UI state for built-in controls.
#[derive(Debug, Clone)]
pub struct ControlUiState {
    /// BUTTON is pressed (between `WM_LBUTTONDOWN` and `WM_LBUTTONUP`).
    pub pressed: bool,
    /// Control has keyboard focus (EDIT etc.).
    pub focused: bool,
    /// BUTTON carries `BS_DEFPUSHBUTTON` — Enter activates it in a dialog.
    pub default_push: bool,
    /// List items (`LISTBOX` / `COMBOBOX`), in insertion order.
    pub items: Vec<String>,
    /// EDIT caret position in characters (0 = before the first character).
    pub caret: usize,
    /// EDIT selection start (character index; == `sel_end` when no selection).
    pub sel_start: usize,
    /// EDIT selection end (exclusive character index).
    pub sel_end: usize,
    /// LISTBOX selected item index (-1 = no selection).
    pub sel_index: i32,
}

impl Default for ControlUiState {
    fn default() -> Self {
        Self {
            pressed: false,
            focused: false,
            default_push: false,
            items: Vec::new(),
            caret: 0,
            sel_start: 0,
            sel_end: 0,
            sel_index: -1,
        }
    }
}

/// Host-side dispatch for a built-in control window.
///
/// `Ok(Some(value))` means the control handled `message` and the guest-visible
/// result is `value`. `Ok(None)` means unhandled — callers fall back to their
/// neutral zero. `Err(..)` carries a [`WinApiControlSignal`] (a nested guest
/// WndProc bridge for `WM_COMMAND`) or a real handler error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_control_proc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    let Some(window) = find_window(state, hwnd) else {
        return Ok(None);
    };
    let Some(kind) = window.control_kind else {
        return Ok(None);
    };
    let unicode = window.unicode;

    match message {
        WM_PAINT => {
            paint_control(state, engine, hwnd, kind)?;
            // Publish the ancestor surface so the painted control becomes
            // visible immediately — the ancestor's own WM_PAINT BitBlt may
            // never run again (a modal dialog otherwise renders as an empty
            // gray box until an unrelated repaint). Mirrors how paint_dialog
            // publishes its face.
            if let Some(ancestor) = resolve_window_ancestor(state, hwnd) {
                state.present().publish(ancestor.hwnd);
            }
            Ok(Some(0))
        }
        WM_LBUTTONDOWN => {
            {
                let ui = control_state_mut(state, hwnd);
                ui.pressed = true;
                if kind != ControlClassKind::Static {
                    ui.focused = true;
                }
            }
            if kind != ControlClassKind::Static {
                // STATIC never takes keyboard focus (labels are not tab stops).
                state.window_state().focus_window_handle = hwnd;
                if kind == ControlClassKind::Button {
                    // Implicit capture: a pushed BUTTON holds the mouse capture
                    // until its WM_LBUTTONUP, so drag-off/release-on still
                    // delivers BN_CLICKED and press-off/drag-on cannot.
                    state.window_state().capture_window_handle = hwnd;
                }
            }
            if kind == ControlClassKind::ListBox {
                // A click on an item row selects it and notifies the parent.
                let clicked = listbox_hit_item(state, hwnd, long_parameter);
                let changed = clicked.is_some_and(|index| {
                    let ui = control_state_mut(state, hwnd);
                    if ui.sel_index == index {
                        return false;
                    }
                    ui.sel_index = index;
                    true
                });
                invalidate(state, hwnd);
                if changed {
                    return listbox_notify_change(state, hwnd);
                }
                return Ok(Some(0));
            }
            invalidate(state, hwnd);
            Ok(Some(0))
        }
        WM_LBUTTONUP => {
            if control_state_mut(state, hwnd).pressed {
                control_state_mut(state, hwnd).pressed = false;
                if kind == ControlClassKind::Button {
                    state.window_state().capture_window_handle = 0;
                }
                invalidate(state, hwnd);
                let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                let command_wparam = (id & 0xFFFF) | (BN_CLICKED << 16);
                return deliver_command(state, hwnd, command_wparam);
            }
            Ok(Some(0))
        }
        // Space on the focused push button presses it; the matching WM_KEYUP
        // (handled below) releases and delivers BN_CLICKED — the keyboard
        // activation path (Windows: space activates the focused button).
        WM_KEYDOWN if kind == ControlClassKind::Button && word_parameter & 0xFF == VK_SPACE => {
            if state.window_state().focus_window_handle == hwnd {
                control_state_mut(state, hwnd).pressed = true;
                invalidate(state, hwnd);
                Ok(Some(0))
            } else {
                Ok(None)
            }
        }
        WM_KEYUP if kind == ControlClassKind::Button && word_parameter & 0xFF == VK_SPACE => {
            if control_state(state, hwnd).is_some_and(|s| s.pressed) {
                control_state_mut(state, hwnd).pressed = false;
                invalidate(state, hwnd);
                let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                let command_wparam = (id & 0xFFFF) | (BN_CLICKED << 16);
                return deliver_command(state, hwnd, command_wparam);
            }
            Ok(Some(0))
        }
        // Programmatic activation: SendMessage(button, BM_CLICK) delivers
        // BN_CLICKED to the parent exactly like a mouse release.
        BM_CLICK if kind == ControlClassKind::Button => {
            control_state_mut(state, hwnd).pressed = false;
            let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
            let command_wparam = (id & 0xFFFF) | (BN_CLICKED << 16);
            deliver_command(state, hwnd, command_wparam)
        }
        // BM_GETSTATE: BST_PUSHED | BST_FOCUS (winuser.h state bits).
        BM_GETSTATE if kind == ControlClassKind::Button => {
            let ui = control_state(state, hwnd).cloned().unwrap_or_default();
            let mut bits = 0;
            if ui.pressed {
                bits |= BST_PUSHED;
            }
            if ui.focused {
                bits |= BST_FOCUS;
            }
            Ok(Some(bits))
        }
        // BM_SETSTATE: set the pressed visual state (no click delivered).
        BM_SETSTATE if kind == ControlClassKind::Button => {
            let ui = control_state_mut(state, hwnd);
            let previous = u64::from(ui.pressed);
            ui.pressed = word_parameter != 0;
            invalidate(state, hwnd);
            Ok(Some(previous))
        }
        WM_SETFOCUS => {
            control_state_mut(state, hwnd).focused = true;
            Ok(Some(0))
        }
        WM_KILLFOCUS => {
            control_state_mut(state, hwnd).focused = false;
            Ok(Some(0))
        }
        WM_GETTEXT => {
            let text = window_text(state, hwnd);
            let count = write_control_text(engine, unicode, long_parameter, word_parameter, &text)?;
            Ok(Some(count))
        }
        WM_GETTEXTLENGTH => {
            let text = window_text(state, hwnd);
            Ok(Some(control_text_length(&text, unicode)))
        }
        WM_SETTEXT => {
            if long_parameter == 0 {
                return Ok(Some(0));
            }
            let text = if unicode {
                read_guest_utf16_lossy(engine, long_parameter, 32_768)?
            } else {
                read_guest_ansi_lossy(engine, long_parameter, 32_768)?
            };
            if let Some(window) = find_window_mut(state, hwnd) {
                window.control_text = text;
                window.invalidated = true;
            }
            Ok(Some(1))
        }
        WM_GETDLGCODE if kind == ControlClassKind::Edit => Ok(Some(DLGC_WANTCHARS)),
        WM_GETDLGCODE if kind == ControlClassKind::Button => {
            let style = find_window(state, hwnd).map_or(0, |w| w.style);
            let push = if style & BS_DEFPUSHBUTTON != 0 {
                DLGC_DEFPUSHBUTTON
            } else {
                DLGC_UNDEFPUSHBUTTON
            };
            Ok(Some(DLGC_BUTTON | push))
        }
        WM_CHAR if kind == ControlClassKind::Edit => {
            let changed = edit_char(state, hwnd, word_parameter);
            if changed {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // Caret navigation keys on a focused EDIT (Shift extends the
        // selection). VK_DELETE has no WM_CHAR, so it is handled below.
        WM_KEYDOWN
            if kind == ControlClassKind::Edit
                && matches!(word_parameter & 0xFF, VK_LEFT | VK_RIGHT | VK_HOME | VK_END) =>
        {
            if edit_move_caret(state, hwnd, word_parameter & 0xFF) {
                invalidate(state, hwnd);
            }
            Ok(Some(0))
        }
        WM_KEYDOWN if kind == ControlClassKind::Edit && word_parameter & 0xFF == VK_DELETE => {
            let changed = edit_delete_at_caret(state, hwnd);
            if changed {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // EM_SETSEL: wParam = start, lParam = end (character positions);
        // a negative argument means "end of text", so (0, -1) selects all.
        EM_SETSEL if kind == ControlClassKind::Edit => {
            let start = low_i32(word_parameter, "EM_SETSEL start")?;
            let end = low_i32(long_parameter, "EM_SETSEL end")?;
            edit_set_selection(state, hwnd, start, end);
            invalidate(state, hwnd);
            Ok(Some(1)) // TRUE
        }
        // EM_GETSEL: optional output pointers (start, end) + packed return
        // MAKELONG(start, end) — low word start, high word end.
        EM_GETSEL if kind == ControlClassKind::Edit => {
            let (start, end) = edit_get_selection(state, hwnd);
            if word_parameter != 0 {
                write_guest_u32(engine, word_parameter, u32::try_from(start).unwrap_or(0))?;
            }
            if long_parameter != 0 {
                write_guest_u32(engine, long_parameter, u32::try_from(end).unwrap_or(0))?;
            }
            let start_lo = u32::try_from(start).unwrap_or(0) & 0xFFFF;
            let end_hi = (u32::try_from(end).unwrap_or(0) & 0xFFFF) << 16;
            Ok(Some(u64::from(start_lo | end_hi)))
        }
        // LB_SETCURSEL: wParam = item index (-1 clears); out-of-range is
        // LB_ERR. A changed selection delivers LBN_SELCHANGE to the parent.
        LB_SETCURSEL if matches!(kind, ControlClassKind::ListBox | ControlClassKind::ComboBox) => {
            let index = low_i32(word_parameter, "LB_SETCURSEL index")?;
            let item_count = control_state(state, hwnd).map_or(0, |s| s.items.len());
            if index >= 0 && usize::try_from(index).unwrap_or(usize::MAX) >= item_count {
                return Ok(Some(u64::MAX)); // LB_ERR: index out of range
            }
            let ui = control_state_mut(state, hwnd);
            let previous = ui.sel_index;
            ui.sel_index = index; // -1 clears the selection
            invalidate(state, hwnd);
            if previous != index {
                return listbox_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // LB_GETCURSEL: the selected index, or LB_ERR when nothing is selected.
        LB_GETCURSEL if matches!(kind, ControlClassKind::ListBox | ControlClassKind::ComboBox) => {
            let selected = control_state(state, hwnd).map_or(-1, |s| s.sel_index);
            if selected < 0 {
                Ok(Some(u64::MAX)) // LB_ERR
            } else {
                Ok(Some(u64::try_from(selected).unwrap_or(0)))
            }
        }
        LB_ADDSTRING if matches!(kind, ControlClassKind::ListBox | ControlClassKind::ComboBox) => {
            if long_parameter == 0 {
                return Ok(Some(u64::MAX)); // LB_ERR
            }
            let text = if unicode {
                read_guest_utf16_lossy(engine, long_parameter, 4096)?
            } else {
                read_guest_ansi_lossy(engine, long_parameter, 4096)?
            };
            let items = &mut control_state_mut(state, hwnd).items;
            items.push(text);
            let index = u64::try_from(items.len().saturating_sub(1)).unwrap_or(u64::MAX);
            invalidate(state, hwnd);
            Ok(Some(index))
        }
        LB_GETCOUNT if matches!(kind, ControlClassKind::ListBox | ControlClassKind::ComboBox) => {
            let count = control_state(state, hwnd).map_or(0, |s| s.items.len());
            Ok(Some(u64::try_from(count).unwrap_or(0)))
        }
        LB_GETTEXT if matches!(kind, ControlClassKind::ListBox | ControlClassKind::ComboBox) => {
            let items = control_state(state, hwnd).map_or_else(Vec::new, |s| s.items.clone());
            let index = usize::try_from(word_parameter).unwrap_or(usize::MAX);
            let Some(item) = items.get(index) else {
                return Ok(Some(u64::MAX)); // LB_ERR
            };
            let count = write_control_text(engine, unicode, long_parameter, 4096, item)?;
            Ok(Some(count))
        }
        WM_COMMAND => deliver_command(state, hwnd, word_parameter),
        _ => Ok(None),
    }
}

/// Deliver `WM_COMMAND` synchronously to the control's parent chain.
///
/// The first ancestor with a guest WndProc — or a modal dialog's dialog proc —
/// receives the command through the established
/// [`WinApiControlSignal::GuestCallbackRequested`] bridge
/// (`OuterReturn::Fixed(0)` completes the outer API). Control-class ancestors
/// (no guest WndProc) are skipped, mirroring how commands bubble to the owning
/// dialog/application window.
fn deliver_command(
    state: &mut WinApiState,
    child_hwnd: u64,
    word_parameter: u64,
) -> Result<Option<u64>> {
    let mut current = find_window(state, child_hwnd).map_or(0, |w| w.parent_handle);
    loop {
        let Some(parent) = find_window(state, current) else {
            return Ok(Some(0));
        };
        if parent.window_proc != 0 || parent.dialog_proc != 0 {
            let id = word_parameter & 0xFFFF;
            let notify = (word_parameter >> 16) & 0xFFFF;
            tracing::info!(
                target: "wiegui",
                id,
                notify,
                child = child_hwnd,
                parent = current,
                "WM_COMMAND delivered"
            );
        }
        if parent.window_proc != 0 {
            return Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: parent.window_proc,
                    window_handle: current,
                    message: WM_COMMAND,
                    word_parameter,
                    long_parameter: child_hwnd,
                    unicode: parent.unicode,
                    outer_return: OuterReturn::Fixed(0),
                },
            }
            .into());
        }
        if parent.dialog_proc != 0 {
            // A button inside a modal dialog commands the dialog proc.
            return Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: parent.dialog_proc,
                    window_handle: current,
                    message: WM_COMMAND,
                    word_parameter,
                    long_parameter: child_hwnd,
                    unicode: parent.dialog_unicode,
                    outer_return: OuterReturn::Fixed(0),
                },
            }
            .into());
        }
        if parent.control_kind.is_none() {
            // Non-control window without a guest WndProc: the command dies.
            return Ok(Some(0));
        }
        current = parent.parent_handle;
    }
}

/// Paint a control into its ancestor's surface at its parent-relative offset.
fn paint_control(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    hwnd: u64,
    kind: ControlClassKind,
) -> Result<()> {
    tracing::trace!(target: "wiegui", kind = ?kind, hwnd, "control paint");
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return Ok(());
    };
    let text = find_window(state, hwnd).map_or_else(String::new, |w| w.control_text.clone());
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    let pressed = control_state(state, hwnd).is_some_and(|s| s.pressed);
    let items = control_state(state, hwnd).map_or_else(Vec::new, |s| s.items.clone());
    let sel_index = control_state(state, hwnd).map_or(-1, |s| s.sel_index);

    // Controls use the system default font (sans-serif 16 px). Take the font
    // engine out of gdi state so it can be passed down with the surface
    // borrows; put it back when the paint is done.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16);
    let result = (|| -> Result<()> {
        let Some(resolved) = &resolved else {
            // No system font: paint faces/borders but skip the text.
            return Ok(());
        };
        match kind {
            ControlClassKind::Button => {
                paint_face_and_border(state, &info, width, height, pressed);
                // The ampersand is a mnemonic marker, not caption glyph.
                let caption = strip_mnemonics(&text);
                let tx = centered_text_x(
                    &info,
                    width,
                    &caption,
                    &mut font_engine,
                    resolved,
                    &default_key,
                );
                paint_label(
                    state,
                    engine,
                    &info,
                    &caption,
                    tx,
                    width,
                    height,
                    pressed,
                    &mut font_engine,
                    resolved,
                    &default_key,
                )?;
            }
            ControlClassKind::Static => {
                // COLOR_BTNFACE, not COLOR_WINDOW: a label sits on the dialog
                // face and must not show as a white box (full WM_CTLCOLOR* is
                // deferred).
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_BTNFACE,
                    false,
                );
                let caption = strip_mnemonics(&text);
                let tx = info.offset_x.saturating_add(2);
                paint_label(
                    state,
                    engine,
                    &info,
                    &caption,
                    tx,
                    width,
                    height,
                    false,
                    &mut font_engine,
                    resolved,
                    &default_key,
                )?;
            }
            ControlClassKind::Edit => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                    false,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                let tx = info.offset_x.saturating_add(2);
                paint_edit(
                    state,
                    engine,
                    &info,
                    &text,
                    tx,
                    width,
                    height,
                    &mut font_engine,
                    resolved,
                    &default_key,
                )?;
            }
            ControlClassKind::ListBox => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                    false,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                paint_item_lines(
                    state,
                    engine,
                    &info,
                    &items,
                    width,
                    height,
                    sel_index,
                    &mut font_engine,
                    resolved,
                    &default_key,
                )?;
            }
            ControlClassKind::ComboBox => {
                paint_face_and_border(state, &info, width, height, false);
                let first = items.first().map_or("", String::as_str);
                let tx = info.offset_x.saturating_add(4);
                paint_label(
                    state,
                    engine,
                    &info,
                    first,
                    tx,
                    width,
                    height,
                    false,
                    &mut font_engine,
                    resolved,
                    &default_key,
                )?;
            }
        }
        Ok(())
    })();
    state.gdi_state().font_engine = font_engine;
    result
}

/// Fill a control's face (COLOR_BTNFACE) and draw its 1 px BTNSHADOW border.
fn paint_face_and_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    pressed: bool,
) {
    let face = if pressed {
        COLOR_BTNFACE_PRESSED
    } else {
        COLOR_BTNFACE
    };
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        width,
        height,
        face,
        false,
    );
    stroke_border(state, info, width, height, COLOR_BTNSHADOW);
}

/// Draw a 1 px border around a control's rect.
fn stroke_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    color: u32,
) {
    if width <= 0 || height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(width).saturating_sub(1);
    let bottom = y.saturating_add(height).saturating_sub(1);
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        width,
        1,
        color,
        false,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        bottom,
        width,
        1,
        color,
        false,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        1,
        height,
        color,
        false,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        right,
        y,
        1,
        height,
        color,
        false,
    );
}

/// Remove `&` mnemonic markers from a caption so they are not rendered
/// literally (the underline + Alt activation are deferred). `&&` is the
/// escaped form of a literal ampersand, matching Windows.
#[must_use]
fn strip_mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '&' {
            if iter.peek() == Some(&'&') {
                out.push('&');
                iter.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// X origin for a horizontally centered single-line control caption.
#[allow(clippy::too_many_arguments)]
fn centered_text_x(
    info: &ResolvedWindow,
    width: i32,
    text: &str,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> i32 {
    let text_w = font_engine.text_advance(resolved, key, text, text.chars().count());
    info.offset_x
        .saturating_add(width.saturating_sub(text_w).saturating_div(2))
}

/// Draw a single line of control text, vertically centered, black on the
/// control's face. `tx` is the caller-computed left edge (centered or padded);
/// the glyphs are clipped to the control's rect.
#[allow(clippy::too_many_arguments)]
fn paint_label(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    text: &str,
    tx: i32,
    width: i32,
    height: i32,
    pressed: bool,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let line_h = resolved.line_height();
    let ty = info
        .offset_y
        .saturating_add(height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y);
    // Pressed buttons offset their caption one pixel down/right (classic 3D).
    let (tx, ty) = if pressed {
        (tx.saturating_add(1), ty.saturating_add(1))
    } else {
        (tx, ty)
    };
    let right = info.offset_x.saturating_add(width);
    let bottom = info.offset_y.saturating_add(height);
    render_control_text(
        state,
        engine,
        info.hwnd,
        info.width,
        info.height,
        tx,
        ty,
        text,
        0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
        Some((info.offset_x, info.offset_y, right, bottom)),
        font_engine,
        resolved,
        key,
    )
}

/// Draw the first visible LISTBOX items (one line each), filling the
/// selected item's row with COLOR_HIGHLIGHT and rendering its glyphs in
/// COLOR_HIGHLIGHTTEXT (Windows' selected-listbox look).
#[allow(clippy::too_many_arguments)]
fn paint_item_lines(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    items: &[String],
    width: i32,
    height: i32,
    sel_index: i32,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(width);
    let bottom = y.saturating_add(height);
    let line_h = resolved.line_height();
    for (index, item) in items.iter().enumerate() {
        let line_y = y.saturating_add(i32::try_from(index).unwrap_or(0).saturating_mul(line_h));
        if line_y >= bottom {
            break;
        }
        let selected = sel_index >= 0 && i32::try_from(index).unwrap_or(-1) == sel_index;
        if selected {
            fill_rect_clipped(
                state,
                info,
                width,
                height,
                x,
                line_y,
                width,
                line_h,
                COLOR_HIGHLIGHT,
            );
        }
        let color = if selected { COLOR_HIGHLIGHTTEXT } else { 0 };
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            x.saturating_add(2),
            line_y,
            item,
            color,
            Some((x, y, right, bottom)),
            font_engine,
            resolved,
            key,
        )?;
    }
    Ok(())
}

/// Render control text into the ancestor surface (TRANSPARENT background).
#[allow(clippy::too_many_arguments)]
fn render_control_text(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    top_hwnd: u64,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    clip: Option<(i32, i32, i32, i32)>,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    state.present().ensure_surface(top_hwnd, width, height);
    let Some(surface) = state.present().surfaces.get_mut(&top_hwnd) else {
        return Ok(());
    };
    render_text_into_surface(
        engine,
        font_engine,
        &mut surface.pixels,
        surface.width,
        surface.height,
        x,
        y,
        text,
        color,
        clip,
        resolved,
        key,
    )
}

/// EDIT: process one `WM_CHAR` — insert at the caret (replacing an active
/// selection), Backspace deletes before the caret. Returns whether the text
/// changed (callers deliver EN_CHANGE only then).
fn edit_char(state: &mut WinApiState, hwnd: u64, char_code: u64) -> bool {
    let ch = u32::try_from(char_code & 0xFFFF).unwrap_or(0);
    let ws = state.window_state();
    let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) else {
        return false;
    };
    let ui = ws.control_states.entry(hwnd).or_default();
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (sel_start, sel_end) = normalized_selection(ui, len);
    match ch {
        0x08 => {
            // VK_BACK: delete the selection, or the character before the caret.
            if sel_start != sel_end {
                replace_range(text, ui, sel_start, sel_end, "");
                return true;
            }
            let caret = ui.caret;
            if caret == 0 {
                return false;
            }
            replace_range(text, ui, caret.saturating_sub(1), caret, "");
            true
        }
        // Enter/Escape are no-ops for the slice; 0x7F (DEL) is handled by the
        // WM_KEYDOWN VK_DELETE path, never inserted as a character.
        0x0D | 0x1B | 0x7F => false,
        _ if ch >= 0x20 => {
            let Some(c) = char::from_u32(ch) else {
                return false;
            };
            let (start, end) = if sel_start == sel_end {
                (ui.caret, ui.caret)
            } else {
                (sel_start, sel_end)
            };
            replace_range(text, ui, start, end, &c.to_string());
            true
        }
        _ => false,
    }
}

/// Replace the character range `[start, end)` (clamped to the text) with
/// `replacement`; the caret lands after the inserted text and the selection is
/// cleared.
fn replace_range(
    text: &mut String,
    ui: &mut ControlUiState,
    start: usize,
    end: usize,
    replacement: &str,
) {
    let len = text.chars().count();
    let start = start.min(len);
    let end = end.min(len).max(start);
    let start_byte = byte_index_of_char(text, start);
    let end_byte = byte_index_of_char(text, end);
    text.replace_range(start_byte..end_byte, replacement);
    ui.caret = start.saturating_add(replacement.chars().count());
    ui.sel_start = ui.caret;
    ui.sel_end = ui.caret;
}

/// Byte offset of the `char_index`-th character (the end of the string when
/// the index is at or past the last character).
#[must_use]
fn byte_index_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The selection as an ordered, clamped `(start, end)` character range.
#[must_use]
fn normalized_selection(ui: &ControlUiState, len: usize) -> (usize, usize) {
    let start = ui.sel_start.min(len);
    let end = ui.sel_end.min(len);
    (start.min(end), start.max(end))
}

/// EDIT: arrow/Home/End caret movement. Shift extends the selection (the
/// anchor stays at the edge the caret moved away from); without Shift the
/// selection collapses. Returns whether the caret/selection moved.
fn edit_move_caret(state: &mut WinApiState, hwnd: u64, vk: u64) -> bool {
    let extend = shift_is_down(state);
    let ws = state.window_state();
    let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) else {
        return false;
    };
    let ui = ws.control_states.entry(hwnd).or_default();
    let len = window.control_text.chars().count();
    let old_caret = ui.caret.min(len);
    let new_caret = match vk {
        VK_LEFT => old_caret.saturating_sub(1),
        VK_RIGHT => old_caret.saturating_add(1).min(len),
        VK_HOME => 0,
        VK_END => len,
        _ => old_caret,
    };
    if new_caret == old_caret && ui.sel_start == ui.sel_end {
        return false;
    }
    if extend {
        // The anchor is the selection edge the caret is not at (or the old
        // caret when the selection was empty).
        let anchor = if ui.sel_start == ui.sel_end {
            old_caret
        } else if old_caret == ui.sel_start.min(ui.sel_end) {
            ui.sel_start.max(ui.sel_end)
        } else {
            ui.sel_start.min(ui.sel_end)
        };
        let (lo, hi) = (anchor.min(new_caret), anchor.max(new_caret));
        ui.sel_start = lo;
        ui.sel_end = hi;
        ui.caret = new_caret;
    } else {
        ui.caret = new_caret;
        ui.sel_start = new_caret;
        ui.sel_end = new_caret;
    }
    true
}

/// EDIT: VK_DELETE — delete the selection, or the character at the caret.
/// Returns whether the text changed.
fn edit_delete_at_caret(state: &mut WinApiState, hwnd: u64) -> bool {
    let ws = state.window_state();
    let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) else {
        return false;
    };
    let ui = ws.control_states.entry(hwnd).or_default();
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (sel_start, sel_end) = normalized_selection(ui, len);
    if sel_start != sel_end {
        replace_range(text, ui, sel_start, sel_end, "");
        return true;
    }
    let caret = ui.caret;
    if caret >= len {
        return false;
    }
    replace_range(text, ui, caret, caret.saturating_add(1), "");
    true
}

/// EDIT: EM_SETSEL — set the selection. A negative argument means "end of
/// text", so `(0, -1)` selects everything; the caret lands at the end edge.
fn edit_set_selection(state: &mut WinApiState, hwnd: u64, start: i32, end: i32) {
    let ws = state.window_state();
    let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) else {
        return;
    };
    let ui = ws.control_states.entry(hwnd).or_default();
    let len = window.control_text.chars().count();
    let start_us = if start < 0 {
        len
    } else {
        usize::try_from(start).unwrap_or(len).min(len)
    };
    let end_us = if end < 0 {
        len
    } else {
        usize::try_from(end).unwrap_or(len).min(len)
    };
    ui.sel_start = start_us.min(end_us);
    ui.sel_end = start_us.max(end_us);
    ui.caret = ui.sel_end;
}

/// EDIT: EM_GETSEL — the current (start, end) character range, normalized.
#[must_use]
fn edit_get_selection(state: &WinApiState, hwnd: u64) -> (usize, usize) {
    control_state(state, hwnd).map_or((0, 0), |ui| {
        (ui.sel_start.min(ui.sel_end), ui.sel_start.max(ui.sel_end))
    })
}

/// Send `EN_CHANGE` as WM_COMMAND(MAKEWPARAM(id, EN_CHANGE)) to the parent —
/// the EDIT text changed (same bubble as BN_CLICKED).
fn edit_notify_change(state: &mut WinApiState, hwnd: u64) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = (id & 0xFFFF) | (EN_CHANGE << 16);
    deliver_command(state, hwnd, command_wparam)
}

/// Send `LBN_SELCHANGE` as WM_COMMAND(MAKEWPARAM(id, LBN_SELCHANGE)) to the
/// parent — the LISTBOX selection changed.
fn listbox_notify_change(state: &mut WinApiState, hwnd: u64) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = (id & 0xFFFF) | (LBN_SELCHANGE << 16);
    deliver_command(state, hwnd, command_wparam)
}

/// Which LISTBOX item row a client-relative click (packed `lParam`) falls on.
/// `None` when the click is outside the item rows or the list is empty.
#[must_use]
fn listbox_hit_item(state: &WinApiState, hwnd: u64, long_parameter: u64) -> Option<i32> {
    let y_raw = u16::try_from((long_parameter >> 16) & 0xFFFF).unwrap_or(0);
    let y = i32::from(i16::from_ne_bytes(y_raw.to_ne_bytes()));
    let count = control_state(state, hwnd).map_or(0, |s| s.items.len());
    if y < 0 || count == 0 {
        return None;
    }
    // One 16 px line per item, flush at the control's top edge.
    let row = y.saturating_div(16);
    if row >= i32::try_from(count).unwrap_or(0) {
        return None;
    }
    Some(row)
}

/// Whether the Shift key is held, per the guest keyboard state.
#[must_use]
fn shift_is_down(state: &WinApiState) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.keyboard_state
            .0
            .get(usize::try_from(VK_SHIFT).unwrap_or(0))
            .is_some_and(|&key| key & 0x80 != 0)
    })
}

/// EDIT paint: text, selection highlight, and the caret bar.
///
/// Glyphs are proportional, so the caret and selection x positions are the
/// SUMMED advances of the preceding characters (matching the rendered text
/// exactly). The selection is drawn in two passes — the whole line in
/// COLOR_WINDOWTEXT, then the selected run re-rendered in
/// COLOR_HIGHLIGHTTEXT over its COLOR_HIGHLIGHT cells. The caret bar (1 px,
/// full line height) is only drawn while the control has focus.
#[allow(clippy::too_many_arguments)]
fn paint_edit(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    text: &str,
    tx: i32,
    width: i32,
    height: i32,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    let len = text.chars().count();
    let ui = control_state(state, info.dc_window).cloned();
    let focused = ui.as_ref().is_some_and(|s| s.focused);
    let (sel_start, sel_end) = ui.as_ref().map_or((0, 0), |s| {
        let (start, end) = (s.sel_start.min(len), s.sel_end.min(len));
        (start.min(end), start.max(end))
    });
    let caret = ui.as_ref().map_or(0, |s| s.caret.min(len));
    let line_h = resolved.line_height();
    let ty = info
        .offset_y
        .saturating_add(height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y);
    let right = info.offset_x.saturating_add(width);
    let bottom = info.offset_y.saturating_add(height);
    let clip = Some((info.offset_x, info.offset_y, right, bottom));
    let has_selection = focused && sel_start != sel_end;

    if has_selection {
        // Pass 1: fill the selected cells with COLOR_HIGHLIGHT (behind text).
        let sel_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, sel_start));
        let sel_w = font_engine
            .text_advance(resolved, key, text, sel_end)
            .saturating_sub(font_engine.text_advance(resolved, key, text, sel_start));
        fill_rect_clipped(
            state,
            info,
            width,
            height,
            sel_x,
            ty,
            sel_w,
            line_h,
            COLOR_HIGHLIGHT,
        );
    }
    // Pass 2: the whole line in the normal text color.
    if !text.is_empty() {
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            tx,
            ty,
            text,
            0,
            clip,
            font_engine,
            resolved,
            key,
        )?;
    }
    if has_selection {
        // Pass 3: re-render the selected run in COLOR_HIGHLIGHTTEXT.
        let selected: String = text
            .chars()
            .skip(sel_start)
            .take(sel_end.saturating_sub(sel_start))
            .collect();
        let sel_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, sel_start));
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            sel_x,
            ty,
            &selected,
            COLOR_HIGHLIGHTTEXT,
            clip,
            font_engine,
            resolved,
            key,
        )?;
    }
    if focused {
        // Pass 4: the 1 px caret bar at the caret's glyph cell.
        let caret_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, caret));
        fill_rect_clipped(
            state,
            info,
            width,
            height,
            caret_x,
            ty,
            1,
            line_h,
            0x0000_0000,
        );
    }
    Ok(())
}

/// Fill a rect with `color`, clipped to the control's own bounds so a
/// selection or caret running past the right edge cannot bleed into the
/// ancestor surface.
#[allow(clippy::too_many_arguments)]
fn fill_rect_clipped(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    control_width: i32,
    control_height: i32,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    color: u32,
) {
    let x0 = x.max(info.offset_x);
    let y0 = y.max(info.offset_y);
    let x1 = x
        .saturating_add(cx)
        .min(info.offset_x.saturating_add(control_width));
    let y1 = y
        .saturating_add(cy)
        .min(info.offset_y.saturating_add(control_height));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x0,
        y0,
        x1.saturating_sub(x0),
        y1.saturating_sub(y0),
        color,
        false,
    );
}

/// Copy a control's text into a guest buffer (WM_GETTEXT / LB_GETTEXT).
fn write_control_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    buffer_ptr: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    if buffer_ptr == 0 {
        return Ok(0);
    }
    let capacity = usize::try_from(max_characters).unwrap_or(0);
    let copied = if unicode {
        write_guest_utf16_c_string(engine, buffer_ptr, capacity, text)?
    } else {
        write_guest_ansi_c_string(engine, buffer_ptr, capacity, text)?
    };
    Ok(u64::try_from(copied).unwrap_or(0))
}

/// WM_GETTEXTLENGTH: the text length in TCHARs.
#[must_use]
fn control_text_length(text: &str, unicode: bool) -> u64 {
    if unicode {
        u64::try_from(text.encode_utf16().count()).unwrap_or(0)
    } else {
        u64::try_from(text.len()).unwrap_or(0)
    }
}

/// The current text of a window (control_text for controls, title otherwise).
fn window_text(state: &WinApiState, hwnd: u64) -> String {
    state
        .try_window_state()
        .and_then(|ws| ws.windows.iter().find(|w| w.handle == hwnd))
        .map_or_else(String::new, |w| {
            if w.control_kind.is_some() {
                w.control_text.clone()
            } else {
                w.title.clone()
            }
        })
}

/// Mutable per-window control UI state (created on demand).
fn control_state_mut(state: &mut WinApiState, hwnd: u64) -> &mut ControlUiState {
    state.window_state().control_states.entry(hwnd).or_default()
}

/// Read-only per-window control UI state.
fn control_state(state: &WinApiState, hwnd: u64) -> Option<&ControlUiState> {
    state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&hwnd))
}

/// Mark a window for a future synthesized WM_PAINT.
fn invalidate(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.invalidated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::strip_mnemonics;

    #[test]
    fn strip_mnemonics_drops_single_marker() {
        assert_eq!(strip_mnemonics("&About"), "About");
        assert_eq!(strip_mnemonics("&Quit"), "Quit");
        assert_eq!(strip_mnemonics("Plain"), "Plain");
    }

    #[test]
    fn strip_mnemonics_keeps_doubled_ampersand() {
        // `&&` is the escaped form and renders as a literal `&`.
        assert_eq!(strip_mnemonics("A&&B"), "A&B");
        assert_eq!(strip_mnemonics("&&"), "&");
        assert_eq!(strip_mnemonics("&A&&B"), "A&B");
    }

    #[test]
    fn strip_mnemonics_empty() {
        assert_eq!(strip_mnemonics(""), "");
    }
}
