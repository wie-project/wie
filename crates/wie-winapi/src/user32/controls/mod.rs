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
    BN_CLICKED, BS_DEFPUSHBUTTON, BST_FOCUS, BST_PUSHED, CommandPayload, DLGC_BUTTON,
    DLGC_DEFPUSHBUTTON, DLGC_UNDEFPUSHBUTTON, DLGC_WANTCHARS, GuestCallbackRequest, VK_DELETE,
    VK_END, VK_HOME, VK_LEFT, VK_RIGHT, VK_SPACE, WM_COMMAND, WinApiControlSignal, WinApiState,
    WinMsg, WindowClassIdentifier, find_window, find_window_mut, low_i32, make_command_wparam,
    read_guest_ansi_lossy, read_guest_utf16_lossy, write_guest_u32,
};
use crate::OuterReturn;
use crate::gdi32::resolve_window_ancestor;
use crate::state::WindowFlags;

mod button;
mod edit;
mod listbox;
mod paint;
mod r#static;

use button::paint_control;
use edit::{
    edit_char, edit_delete_at_caret, edit_get_selection, edit_move_caret, edit_notify_change,
    edit_set_selection,
};
use listbox::{listbox_hit_item, listbox_notify_change};
use paint::write_control_text;

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

    /// The initial per-kind control state (what the flat struct's defaults
    /// seeded: nothing pressed/selected, empty item list, caret at 0).
    #[must_use]
    pub(crate) fn new_state(self) -> ControlState {
        match self {
            Self::Button => ControlState::Button {
                default_push: false,
            },
            Self::Edit => ControlState::Edit {
                caret: 0,
                sel_start: 0,
                sel_end: 0,
            },
            Self::ListBox => ControlState::ListBox {
                items: Vec::new(),
                sel_index: -1,
            },
            Self::ComboBox => ControlState::ComboBox {
                items: Vec::new(),
                sel_index: -1,
            },
            Self::Static => ControlState::Static,
        }
    }
}

/// Per-kind runtime UI state for built-in controls.
///
/// Each variant carries only the bits its control uses, so reading `caret`
/// on a `Button` state is a compile error instead of a silent zero. The
/// window-generic interaction bits (mouse `pressed`, keyboard `focused`)
/// live on `WindowRecord` — they are written by kind-agnostic input-message
/// arms for every control.
#[derive(Debug, Clone)]
pub enum ControlState {
    /// BUTTON (push buttons).
    Button {
        /// Carries `BS_DEFPUSHBUTTON` — Enter activates it in a dialog.
        default_push: bool,
    },
    /// EDIT (single/multi-line text input).
    Edit {
        /// Caret position in characters (0 = before the first character).
        caret: usize,
        /// Selection start (character index; == `sel_end` when no selection).
        sel_start: usize,
        /// Selection end (exclusive character index).
        sel_end: usize,
    },
    /// LISTBOX (item list, no scrollbar yet).
    ListBox {
        /// List items, in insertion order.
        items: Vec<String>,
        /// Selected item index (-1 = no selection).
        sel_index: i32,
    },
    /// COMBOBOX (edit+list; no dropdown yet).
    ComboBox {
        /// List items, in insertion order.
        items: Vec<String>,
        /// Selected item index (-1 = no selection).
        ///
        /// The pre-P4 code accepted the LISTBOX selection messages for the
        /// combo too, so the combo tracks a selection to stay byte-identical.
        sel_index: i32,
    },
    /// STATIC (labels; text-only painting).
    Static,
}

impl ControlState {
    /// Whether this is a `BS_DEFPUSHBUTTON` button (the dialog's Enter target).
    #[must_use]
    pub fn is_default_push(&self) -> bool {
        matches!(
            self,
            Self::Button {
                default_push: true,
                ..
            }
        )
    }

    /// Set/reset the `BS_DEFPUSHBUTTON` flag (a no-op on non-Button states).
    pub fn set_default_push(&mut self, value: bool) {
        if let Self::Button { default_push, .. } = self {
            *default_push = value;
        }
    }
}

/// Host-side dispatch for a built-in control window.
///
/// `Ok(Some(value))` means the control handled `message` and the guest-visible
/// result is `value`. `Ok(None)` means unhandled — callers fall back to their
/// neutral zero. `Err(..)` carries a [`WinApiControlSignal`] (a nested guest
/// WndProc bridge for `WM_COMMAND`) or a real handler error.
pub(crate) fn dispatch_control_proc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    let Some(kind) = find_window(state, hwnd).and_then(|w| w.control_kind) else {
        return Ok(None);
    };
    kind.dispatch(engine, state, hwnd, message, word_parameter, long_parameter)
}

impl ControlClassKind {
    /// The typed per-kind dispatch: matches `(self, WinMsg::from(message))` so
    /// each arm reads only its own variant's fields (reading `caret` on a
    /// `Button` state is a compile error).
    pub(crate) fn dispatch(
        self,
        engine: &mut dyn wie_cpu::CpuEngine,
        state: &mut WinApiState,
        hwnd: u64,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
    ) -> Result<Option<u64>> {
        let unicode = find_window(state, hwnd).is_some_and(|w| w.unicode);
        match (self, WinMsg::from(message)) {
            (_, WinMsg::WM_PAINT) => {
                paint_control(state, engine, hwnd, self)?;
                // Publish the ancestor surface so the painted control becomes
                // visible immediately — the ancestor's own WM_PAINT BitBlt may
                // never run again (a modal dialog otherwise renders as an empty
                // gray box until an unrelated repaint). Mirrors how paint_dialog
                // publishes its face. B3.6: deferred — the runtime drains
                // pending publishes once per repaint cycle (at the empty-queue
                // idle boundary), so a repaint cycle (parent BitBlt + each
                // child paint) emits one frame with the union region.
                if let Some(ancestor) = resolve_window_ancestor(state, hwnd) {
                    state.present().publish_deferred(ancestor.hwnd);
                }
                Ok(Some(0))
            }
            (_, WinMsg::WM_LBUTTONDOWN) => {
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.flags.insert(WindowFlags::PRESSED);
                    if self != ControlClassKind::Static {
                        window.flags.insert(WindowFlags::FOCUSED);
                    }
                }
                if self != ControlClassKind::Static {
                    // STATIC never takes keyboard focus (labels are not tab stops).
                    state.window_state().focus_window_handle = crate::handles::Hwnd::from(hwnd);
                    if self == ControlClassKind::Button {
                        // Implicit capture: a pushed BUTTON holds the mouse capture
                        // until its WM_LBUTTONUP, so drag-off/release-on still
                        // delivers BN_CLICKED and press-off/drag-on cannot.
                        state.window_state().capture_window_handle =
                            crate::handles::Hwnd::from(hwnd);
                    }
                }
                if self == ControlClassKind::ListBox {
                    // A click on an item row selects it and notifies the parent.
                    let clicked = listbox_hit_item(state, hwnd, long_parameter);
                    let changed = clicked.is_some_and(|index| {
                        let ControlState::ListBox { sel_index, .. } =
                            control_state_mut(state, hwnd)
                        else {
                            return false;
                        };
                        if *sel_index == index {
                            return false;
                        }
                        *sel_index = index;
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
            (_, WinMsg::WM_LBUTTONUP) => {
                let was_pressed = find_window_mut(state, hwnd).is_some_and(|window| {
                    if window.flags.contains(WindowFlags::PRESSED) {
                        window.flags.remove(WindowFlags::PRESSED);
                        true
                    } else {
                        false
                    }
                });
                if was_pressed {
                    if self == ControlClassKind::Button {
                        state.window_state().capture_window_handle = crate::handles::Hwnd::NULL;
                    }
                    invalidate(state, hwnd);
                    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                    let command_wparam = make_command_wparam(id, BN_CLICKED);
                    return deliver_command(state, hwnd, command_wparam);
                }
                Ok(Some(0))
            }
            // Space on the focused push button presses it; the matching WM_KEYUP
            // (handled below) releases and delivers BN_CLICKED — the keyboard
            // activation path (Windows: space activates the focused button).
            (ControlClassKind::Button, WinMsg::WM_KEYDOWN) if word_parameter & 0xFF == VK_SPACE => {
                if state.window_state().focus_window_handle == crate::handles::Hwnd::from(hwnd) {
                    if let Some(window) = find_window_mut(state, hwnd) {
                        window.flags.insert(WindowFlags::PRESSED);
                    }
                    invalidate(state, hwnd);
                    Ok(Some(0))
                } else {
                    Ok(None)
                }
            }
            (ControlClassKind::Button, WinMsg::WM_KEYUP) if word_parameter & 0xFF == VK_SPACE => {
                if find_window(state, hwnd).is_some_and(|w| w.flags.contains(WindowFlags::PRESSED))
                {
                    if let Some(window) = find_window_mut(state, hwnd) {
                        window.flags.remove(WindowFlags::PRESSED);
                    }
                    invalidate(state, hwnd);
                    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                    let command_wparam = make_command_wparam(id, BN_CLICKED);
                    return deliver_command(state, hwnd, command_wparam);
                }
                Ok(Some(0))
            }
            // Programmatic activation: SendMessage(button, BM_CLICK) delivers
            // BN_CLICKED to the parent exactly like a mouse release.
            (ControlClassKind::Button, WinMsg::BM_CLICK) => {
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.flags.remove(WindowFlags::PRESSED);
                }
                let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                let command_wparam = make_command_wparam(id, BN_CLICKED);
                deliver_command(state, hwnd, command_wparam)
            }
            // BM_GETSTATE: BST_PUSHED | BST_FOCUS (winuser.h state bits).
            (ControlClassKind::Button, WinMsg::BM_GETSTATE) => {
                let window = find_window(state, hwnd);
                let mut bits = 0;
                if window.is_some_and(|w| w.flags.contains(WindowFlags::PRESSED)) {
                    bits |= BST_PUSHED;
                }
                if window.is_some_and(|w| w.flags.contains(WindowFlags::FOCUSED)) {
                    bits |= BST_FOCUS;
                }
                Ok(Some(bits))
            }
            // BM_SETSTATE: set the pressed visual state (no click delivered).
            (ControlClassKind::Button, WinMsg::BM_SETSTATE) => {
                let previous = find_window_mut(state, hwnd).is_some_and(|window| {
                    let prev = window.flags.contains(WindowFlags::PRESSED);
                    if word_parameter != 0 {
                        window.flags.insert(WindowFlags::PRESSED);
                    } else {
                        window.flags.remove(WindowFlags::PRESSED);
                    }
                    prev
                });
                let previous = u64::from(previous);
                invalidate(state, hwnd);
                Ok(Some(previous))
            }
            (_, WinMsg::WM_SETFOCUS) => {
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.flags.insert(WindowFlags::FOCUSED);
                }
                Ok(Some(0))
            }
            (_, WinMsg::WM_KILLFOCUS) => {
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.flags.remove(WindowFlags::FOCUSED);
                }
                Ok(Some(0))
            }
            (_, WinMsg::WM_GETTEXT) => {
                let text = window_text(state, hwnd);
                let count =
                    write_control_text(engine, unicode, long_parameter, word_parameter, &text)?;
                Ok(Some(count))
            }
            (_, WinMsg::WM_GETTEXTLENGTH) => {
                let text = window_text(state, hwnd);
                Ok(Some(control_text_length(&text, unicode)))
            }
            (_, WinMsg::WM_SETTEXT) => {
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
            (ControlClassKind::Edit, WinMsg::WM_GETDLGCODE) => Ok(Some(DLGC_WANTCHARS)),
            (ControlClassKind::Button, WinMsg::WM_GETDLGCODE) => {
                let style = find_window(state, hwnd).map_or(0, |w| w.style);
                let push = if style & BS_DEFPUSHBUTTON != 0 {
                    DLGC_DEFPUSHBUTTON
                } else {
                    DLGC_UNDEFPUSHBUTTON
                };
                Ok(Some(DLGC_BUTTON | push))
            }
            (ControlClassKind::Edit, WinMsg::WM_CHAR) => {
                let changed = edit_char(state, hwnd, word_parameter);
                if changed {
                    invalidate(state, hwnd);
                    return edit_notify_change(state, hwnd);
                }
                Ok(Some(0))
            }
            // Caret navigation keys on a focused EDIT (Shift extends the
            // selection). VK_DELETE has no WM_CHAR, so it is handled below.
            (ControlClassKind::Edit, WinMsg::WM_KEYDOWN)
                if matches!(word_parameter & 0xFF, VK_LEFT | VK_RIGHT | VK_HOME | VK_END) =>
            {
                if edit_move_caret(state, hwnd, word_parameter & 0xFF) {
                    invalidate(state, hwnd);
                }
                Ok(Some(0))
            }
            (ControlClassKind::Edit, WinMsg::WM_KEYDOWN) if word_parameter & 0xFF == VK_DELETE => {
                let changed = edit_delete_at_caret(state, hwnd);
                if changed {
                    invalidate(state, hwnd);
                    return edit_notify_change(state, hwnd);
                }
                Ok(Some(0))
            }
            // EM_SETSEL: wParam = start, lParam = end (character positions);
            // a negative argument means "end of text", so (0, -1) selects all.
            (ControlClassKind::Edit, WinMsg::EM_SETSEL) => {
                let start = low_i32(word_parameter, "EM_SETSEL start")?;
                let end = low_i32(long_parameter, "EM_SETSEL end")?;
                edit_set_selection(state, hwnd, start, end);
                invalidate(state, hwnd);
                Ok(Some(1)) // TRUE
            }
            // EM_GETSEL: optional output pointers (start, end) + packed return
            // MAKELONG(start, end) — low word start, high word end.
            (ControlClassKind::Edit, WinMsg::EM_GETSEL) => {
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
            // LB_SETCURSEL / CB_SETCURSEL: wParam = item index (-1 clears);
            // out-of-range is LB_ERR. A changed selection delivers
            // LBN_SELCHANGE to the parent.
            (
                ControlClassKind::ListBox | ControlClassKind::ComboBox,
                WinMsg::LB_SETCURSEL | WinMsg::CB_SETCURSEL,
            ) => {
                let index = low_i32(word_parameter, "LB_SETCURSEL index")?;
                let item_count = control_items(state, hwnd).len();
                if index >= 0 && usize::try_from(index).unwrap_or(usize::MAX) >= item_count {
                    return Ok(Some(u64::MAX)); // LB_ERR: index out of range
                }
                let previous = match control_state_mut(state, hwnd) {
                    ControlState::ListBox { sel_index, .. }
                    | ControlState::ComboBox { sel_index, .. } => {
                        let prev = *sel_index;
                        *sel_index = index; // -1 clears the selection
                        prev
                    }
                    _ => -1,
                };
                invalidate(state, hwnd);
                if previous != index {
                    return listbox_notify_change(state, hwnd);
                }
                Ok(Some(0))
            }
            // LB_GETCURSEL / CB_GETCURSEL: the selected index, or LB_ERR
            // when nothing is selected.
            (
                ControlClassKind::ListBox | ControlClassKind::ComboBox,
                WinMsg::LB_GETCURSEL | WinMsg::CB_GETCURSEL,
            ) => {
                let selected = control_sel_index(state, hwnd);
                if selected < 0 {
                    Ok(Some(u64::MAX)) // LB_ERR
                } else {
                    Ok(Some(u64::try_from(selected).unwrap_or(0)))
                }
            }
            (
                ControlClassKind::ListBox | ControlClassKind::ComboBox,
                WinMsg::LB_ADDSTRING | WinMsg::CB_ADDSTRING,
            ) => {
                if long_parameter == 0 {
                    return Ok(Some(u64::MAX)); // LB_ERR
                }
                let text = if unicode {
                    read_guest_utf16_lossy(engine, long_parameter, 4096)?
                } else {
                    read_guest_ansi_lossy(engine, long_parameter, 4096)?
                };
                let (ControlState::ListBox { items, .. } | ControlState::ComboBox { items, .. }) =
                    control_state_mut(state, hwnd)
                else {
                    return Ok(Some(u64::MAX));
                };
                items.push(text);
                let index = u64::try_from(items.len().saturating_sub(1)).unwrap_or(u64::MAX);
                invalidate(state, hwnd);
                Ok(Some(index))
            }
            (
                ControlClassKind::ListBox | ControlClassKind::ComboBox,
                WinMsg::LB_GETCOUNT | WinMsg::CB_GETCOUNT,
            ) => {
                let count = control_items(state, hwnd).len();
                Ok(Some(u64::try_from(count).unwrap_or(0)))
            }
            (
                ControlClassKind::ListBox | ControlClassKind::ComboBox,
                WinMsg::LB_GETTEXT | WinMsg::CB_GETLBTEXT,
            ) => {
                let items = control_items(state, hwnd).to_vec();
                let index = usize::try_from(word_parameter).unwrap_or(usize::MAX);
                let Some(item) = items.get(index) else {
                    return Ok(Some(u64::MAX)); // LB_ERR
                };
                let count = write_control_text(engine, unicode, long_parameter, 4096, item)?;
                Ok(Some(count))
            }
            (_, WinMsg::WM_COMMAND) => deliver_command(state, hwnd, word_parameter),
            _ => Ok(None),
        }
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
    let mut current = find_window(state, child_hwnd).map_or(0, |w| w.parent_handle.as_u64());
    loop {
        let Some(parent) = find_window(state, current) else {
            return Ok(Some(0));
        };
        if parent.window_proc != 0 || parent.dialog_proc != 0 {
            let command = CommandPayload::decode(word_parameter, child_hwnd);
            let id = command.id;
            let notify = command.notify;
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
        current = parent.parent_handle.as_u64();
    }
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
        .and_then(|ws| {
            ws.windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        })
        .map_or_else(String::new, |w| {
            if w.control_kind.is_some() {
                w.control_text.clone()
            } else {
                w.title.clone()
            }
        })
}

/// Mutable per-window control UI state, seeded with the window's kind (the
/// kind is always `Some` at the call sites — the dispatch returns early when a
/// window has no `control_kind`).
fn control_state_mut(state: &mut WinApiState, hwnd: u64) -> &mut ControlState {
    let kind = find_window(state, hwnd)
        .and_then(|w| w.control_kind)
        .unwrap_or(ControlClassKind::Static);
    state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| kind.new_state())
}
/// Read-only per-window control UI state.
fn control_state(state: &WinApiState, hwnd: u64) -> Option<&ControlState> {
    state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
}

/// The item list of a LISTBOX/COMBOBOX state (empty for other kinds).
#[must_use]
fn control_items(state: &WinApiState, hwnd: u64) -> &[String] {
    match control_state(state, hwnd) {
        Some(ControlState::ListBox { items, .. } | ControlState::ComboBox { items, .. }) => items,
        _ => &[],
    }
}

/// The selected index of a LISTBOX/COMBOBOX state (-1 for other kinds).
#[must_use]
fn control_sel_index(state: &WinApiState, hwnd: u64) -> i32 {
    match control_state(state, hwnd) {
        Some(
            ControlState::ListBox { sel_index, .. } | ControlState::ComboBox { sel_index, .. },
        ) => *sel_index,
        _ => -1,
    }
}

/// Mark a window for a future synthesized WM_PAINT.
fn invalidate(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.invalidated = true;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn new_state_seeds_the_right_variant_per_kind() {
        use super::{ControlClassKind, ControlState};
        assert!(matches!(
            ControlClassKind::Button.new_state(),
            ControlState::Button {
                default_push: false
            }
        ));
        assert!(matches!(
            ControlClassKind::Edit.new_state(),
            ControlState::Edit {
                caret: 0,
                sel_start: 0,
                sel_end: 0
            }
        ));
        assert!(matches!(
            ControlClassKind::ListBox.new_state(),
            ControlState::ListBox { sel_index: -1, .. }
        ));
        assert!(matches!(
            ControlClassKind::ComboBox.new_state(),
            ControlState::ComboBox { sel_index: -1, .. }
        ));
        assert!(matches!(
            ControlClassKind::Static.new_state(),
            ControlState::Static
        ));
    }
}
