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
    DLGC_DEFPUSHBUTTON, DLGC_UNDEFPUSHBUTTON, EN_VSCROLL, GuestCallbackRequest, VK_DOWN, VK_SPACE,
    VK_UP, WM_COMMAND, WinApiControlSignal, WinApiState, WinMsg, WindowClassIdentifier,
    find_window, find_window_mut, low_i32, make_command_wparam, read_guest_ansi_lossy,
    read_guest_utf16_lossy,
};
use crate::OuterReturn;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::{FontEngine, FontKey, IRect, ResolvedFont};
use crate::state::WindowFlags;

mod button;
mod edit;
mod listbox;
mod paint;
mod r#static;

// ── Paint-context bundles ───────────────────────────────────────────────
//
// The control paint/hit-test signatures bundle their recurring parameter
// groups so they stay under the clippy `too_many_arguments` limit without
// splitting the drawing call sites.

/// Bundled engine + state for the control paint/hit-test paths.
pub(super) struct PaintCtx<'a> {
    pub state: &'a mut WinApiState,
    pub engine: &'a mut dyn wie_cpu::CpuEngine,
}

/// The active font for one paint call — the engine, the resolved face, and
/// its key always travel together.
pub(super) struct PaintFont<'a> {
    pub engine: &'a mut FontEngine,
    pub resolved: &'a ResolvedFont,
    pub key: &'a FontKey,
}

/// A control's client extent — the width/height pair every paint path
/// resolves from the window record (bundle for the paint signatures).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Dimension {
    pub width: i32,
    pub height: i32,
}

/// The text-layout geometry a paint call draws into.
pub(super) struct TextGeom {
    pub tx: i32,
    pub width: i32,
    pub height: i32,
}

/// The wrap-aware layout parameters of one EDIT hit-test call.
///
/// `pub(crate)` (not `pub(super)` like the other bundles): the host unit tests
/// in `state/tests.rs` construct it directly at the `edit_char_index_at_point`
/// call sites.
pub(crate) struct HitTestLayout {
    pub wrap_width: i32,
    pub line_height: i32,
    pub first_visible: usize,
    pub wrap: bool,
    pub alignment: u32,
}

use button::{button_invalidate_pressed, label_reset_invalid_full, paint_control};
// The text-change invalidation is also called from the SetWindowText handlers
// in `user32::window` (they write control text outside the control dispatch).
pub(crate) use button::label_invalidate_text_change;
/// `UndoSnapshot` is the type of the public `ControlState::Edit::undo_snapshot`
/// field, so it must be reachable at the same visibility as the enum.
pub use edit::UndoSnapshot;
// The EDIT dispatch arms live in `edit::messages` (`dispatch_edit_message`);
// `controls` only keeps the cross-cutting call sites: the generic WM_LBUTTONUP
// arm ends an EDIT's drag session, and the WM_SETFONT / WM_SETTEXT arms reset
// the EDIT's pending row band / buffer state outside the control dispatch.
use edit::{
    dispatch_edit_message, edit_invalidate_text_buffer, edit_mouse_up, edit_notify_scroll,
    edit_reset_invalid_rows,
};
use listbox::{
    listbox_hit_item, listbox_invalidate_appended, listbox_invalidate_selection,
    listbox_key_move_selection, listbox_notify_change, listbox_scroll_wheel,
};
// Re-exported for the comdlg32 dialog build (`open_host_font_dialog` seeds
// the font-dialog listboxes by scrolling the initial selection into view).
pub(crate) use listbox::listbox_scroll_selection_into_view;
use paint::write_control_text;
// Re-exported for the host unit tests in `state/tests.rs` (the `edit` module
// itself stays private to `controls`); test-only so the lib build has no
// unused import.
#[cfg(test)]
pub(crate) use edit::{
    VisibleSegment, clamp_scroll_offset, edit_char_index_at_point, edit_text_area,
    layout_visible_lines, scrollbar_visible, visible_line_count, visual_rows,
};
// The no-create undo-buffer clear is called from the SetWindowText handlers
// in `user32::window` (they write control text outside the control dispatch).
pub(crate) use edit::edit_clear_undo_buffer;

/// `GetSysColor(COLOR_BTNFACE)` — the standard push-button face.
const COLOR_BTNFACE: u32 = 0x00F0_F0F0;
/// `GetSysColor(COLOR_BTNSHADOW)` — the standard button border gray.
const COLOR_BTNSHADOW: u32 = 0x00A0_A0A0;
/// `GetSysColor(COLOR_BTNHIGHLIGHT)` — the classic light edge of a raised
/// 3D border (the status bar's top client edge).
const COLOR_BTNHIGHLIGHT: u32 = 0x00FF_FFFF;
/// `GetSysColor(COLOR_WINDOW)` — the standard EDIT / LISTBOX background.
const COLOR_WINDOW: u32 = 0x00FF_FFFF;
/// Slightly darker face while a button is pressed (matches the classic 3D
/// pressed look).
const COLOR_BTNFACE_PRESSED: u32 = 0x00D8_D8D8;
/// `GetSysColor(COLOR_HIGHLIGHT)` — the selection fill for EDIT / LISTBOX.
const COLOR_HIGHLIGHT: u32 = 0x0000_78D7;
/// `GetSysColor(COLOR_HIGHLIGHTTEXT)` — text drawn over the selection.
const COLOR_HIGHLIGHTTEXT: u32 = 0x00FF_FFFF;

/// `ES_MULTILINE` — the EDIT accepts `\n` and answers the EM_* line metrics.
///
/// The value is the REAL Windows `ES_MULTILINE` (winuser.h 0x0004) — the
/// creation `dwStyle` flows through unchanged, so a mingw-compiled guest's
/// ES_* bits must match. (0x1000 is `ES_WANTRETURN`, the value this constant
/// was once wrongly set to, which made every real multiline EDIT look
/// single-line: Enter inserted nothing and the paint vertically centered.)
///
/// The full ES_* style set (ES_MULTILINE 0x4, ES_WANTRETURN 0x1000,
/// ES_AUTOVSCROLL 0x40, ES_AUTOHSCROLL 0x80, ES_NOHIDESEL 0x100, ES_READONLY
/// 0x800, plus the ES_LEFT/CENTER/RIGHT 0x3 alignment mask) is captured
/// wholesale into `ControlState::Edit::style_bits` at seed time; only the bits
/// a task reads get a named constant here.
pub(crate) const ES_MULTILINE: u32 = 0x0004;

// ── Status-bar (SB_*) messages (commctrl.h) ─────────────────────────────
//
// The status bar is a comctl32 control, so its messages are WM_USER+ offsets
// rather than `WinMsg` variants, and the dispatch matches them on the raw
// value. Modern comctl32 SPLITS SB_SETTEXT into A (WM_USER+1) and W
// (WM_USER+11) variants — notepad's `SendMessageW(hStatusBar, SB_SETTEXTW, …)`
// sends 0x040B, NOT the plan's legacy single-variant 0x0401 (verified against
// the mingw-w64 14 commctrl.h the guest toolchain builds against).
pub(crate) const SB_SETTEXTA: u32 = 0x0401; // WM_USER+1
pub(crate) const SB_GETTEXTA: u32 = 0x0402; // WM_USER+2
pub(crate) const SB_GETTEXTLENGTHA: u32 = 0x0403; // WM_USER+3
pub(crate) const SB_SETPARTS: u32 = 0x0404; // WM_USER+4
pub(crate) const SB_GETPARTS: u32 = 0x0406; // WM_USER+6
pub(crate) const SB_GETTEXTLENGTHW: u32 = 0x040C; // WM_USER+12
pub(crate) const SB_SETTEXTW: u32 = 0x040B; // WM_USER+11
pub(crate) const SB_GETTEXTW: u32 = 0x040D; // WM_USER+13

/// `SBT_NOBORDERS` — an SB_SETTEXT flag OR'd into the part index (ignored).
pub(crate) const SBT_NOBORDERS: u32 = 0x0100;
/// `SBT_POPOUT` — an SB_SETTEXT flag OR'd into the part index (ignored).
pub(crate) const SBT_POPOUT: u32 = 0x0200;
/// `SBT_OWNERDRAW` — an SB_SETTEXT flag OR'd into the part index (ignored).
pub(crate) const SBT_OWNERDRAW: u32 = 0x1000;

// ── Common-control styles (commctrl.h) for the status bar ───────────────
/// `CCS_BOTTOM` — the control aligns to the bottom of its parent (the
/// notepad status bar's alignment); any other alignment value aligns to the
/// top edge (CCS_TOP = 0x1 is the classic top-alignment constant).
pub(crate) const CCS_BOTTOM: u32 = 0x0000_0003;
/// `CCS_NORESIZE` — WM_SIZE must not change the control's width.
pub(crate) const CCS_NORESIZE: u32 = 0x0000_0004;
/// `CCS_NOPARENTALIGN` — WM_SIZE must not move the control inside its parent.
pub(crate) const CCS_NOPARENTALIGN: u32 = 0x0000_0008;

// `EM_SELECTIONTYPE` return bits (winuser.h). SEL_ATTRIBUTE (0x2) and
// SEL_RECHANGE (0x4) are rich-edit-only and never set by a plain EDIT.
pub(crate) const SEL_EMPTY: u64 = 0x0000;
pub(crate) const SEL_TEXT: u64 = 0x0001;
pub(crate) const SEL_MULTICHAR: u64 = 0x0008;
pub(crate) const SEL_MULTILINE: u64 = 0x0010;

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
    /// `msctls_statusbar32` (STATUSCLASSNAME) — COMCTL32 status bar.
    ///
    /// Task 3.1: the SB_* messages and full painting (per-part text on a
    /// raised BTNFACE strip) are implemented; the paint and message state
    /// live in `ControlState::StatusBar` + `comctl32.rs`.
    StatusBar,
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
                    // STATUSCLASSNAMEW — comctl32's built-in status bar class.
                    // CreateStatusWindowA/W always pass it by name; ordinal
                    // resolution is deferred until a guest is seen passing a
                    // MAKEINTRESOURCE atom for it.
                    "MSCTLS_STATUSBAR32" => Some(Self::StatusBar),
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
                // A fresh control's content is undefined — the first paint
                // must cover everything, so the seed is Full (a mutation mark
                // before the first paint keeps it Full).
                invalidation: LabelInvalidation::Full,
            },
            // No window context here, so the style bits seed to zero; the
            // EDIT's own seeder (`edit_state_mut`) captures the real dwStyle.
            Self::Edit => Self::new_edit_state(0),
            Self::ListBox => ControlState::ListBox {
                items: Vec::new(),
                sel_index: -1,
                first_visible: 0,
                // Same undefined-content seed as the Button/Static variants: the
                // first paint must cover everything.
                invalidation: LabelInvalidation::Full,
            },
            Self::ComboBox => ControlState::ComboBox {
                items: Vec::new(),
                sel_index: -1,
            },
            Self::StatusBar => ControlState::StatusBar {
                part_rights: Vec::new(),
                part_texts: Vec::new(),
            },
            Self::Static => ControlState::Static {
                // Same undefined-content seed as the Button variant.
                invalidation: LabelInvalidation::Full,
            },
        }
    }

    /// The initial EDIT state with the window's creation style captured into
    /// `style_bits` (the ES_* bits live in dwStyle's low word; the WS_* bits
    /// ride along harmlessly).
    #[must_use]
    pub(crate) fn new_edit_state(style: u32) -> ControlState {
        ControlState::Edit {
            caret: 0,
            sel_start: 0,
            sel_end: 0,
            goal_column: None,
            style_bits: style,
            limit: 0,
            modified: false,
            handle_buffer: 0,
            undo_snapshot: None,
            first_visible_line: 0,
            first_visible_column: 0,
            scrollbar_drag: None,
            tab_stops: Vec::new(),
            caret_on: true,
            last_caret_drawn_row: None,
            invalid_rows: EditInvalidation::Clean,
            last_paint_rows: 0,
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
        /// The repaint scope for the next paint (see [`LabelInvalidation`]):
        /// `Clean`/`Full` repaint the whole client, `Rect` only the dirty
        /// sub-rect. Set by the mutating ops (a pressed-state change marks
        /// the face rect, a text change the caption rect), reset to `Full`
        /// by structural changes (WM_SETFONT, a resize), and consumed (back
        /// to `Clean`) by the button paint.
        invalidation: LabelInvalidation,
    },
    /// EDIT (single/multi-line text input).
    Edit {
        /// Caret position in characters (0 = before the first character).
        caret: usize,
        /// Selection start (character index; == `sel_end` when no selection).
        sel_start: usize,
        /// Selection end (exclusive character index).
        sel_end: usize,
        /// The vertical-movement goal column (character offset within a line).
        /// `Some` while a VK_UP/DOWN/PgUp/PgDn sequence is in progress, so a
        /// later press returns to the remembered column once a longer line is
        /// reached (Windows column-memory semantics); `None` means "use the
        /// caret's current column". Set on vertical keys, cleared by horizontal
        /// movement and any text mutation.
        goal_column: Option<usize>,
        /// The window's creation `dwStyle` captured at first use. The ES_*
        /// bits (ES_MULTILINE 0x4, ES_WANTRETURN 0x1000, ES_AUTOVSCROLL 0x40,
        /// ES_AUTOHSCROLL 0x80, ES_NOHIDESEL 0x100, ES_READONLY 0x800, the
        /// ES_LEFT/CENTER/RIGHT 0x3 alignment mask) sit in the low word;
        /// WS_* bits ride along harmlessly.
        style_bits: u32,
        /// `EM_LIMITTEXT` cap in characters (0 = unlimited).
        limit: usize,
        /// `EM_GETMODIFY`/`EM_SETMODIFY` flag; set on any text mutation.
        modified: bool,
        /// The cached `EM_GETHANDLE` buffer (a guest VA the guest owns and
        /// LocalFree's). Reused on repeat GETHANDLE calls while the text is
        /// unchanged; any text mutation clears it so the next GETHANDLE
        /// allocates a fresh copy.
        handle_buffer: u64,
        /// The single-level undo snapshot (Task 2.6): the text, caret, and
        /// selection captured before the last mutation. `EM_UNDO`/`WM_UNDO`
        /// restore it and clear the buffer; `EM_CANUNDO` reports whether one
        /// is pending; `EM_EMPTYUNDOBUFFER` discards it. `None` = nothing to
        /// undo (fresh control, or after an undo/empty).
        undo_snapshot: Option<UndoSnapshot>,
        /// First visible line (`EM_GETFIRSTVISIBLELINE`; `EM_SCROLLCARET`
        /// updates it). Task 2.2's paint reads it as the visual-row start
        /// (`layout_visible_lines`' `first_visible`); Task 2.4: it currently
        /// also serves as the vertical scroll offset.
        first_visible_line: usize,
        /// Horizontal scroll offset in px for a wrap-off EDIT (WS_HSCROLL):
        /// the row text is shifted left by this many pixels. Driven by
        /// WM_HSCROLL and the host scrollbar thumb drag; 0 while the content
        /// fits (or the EDIT wraps).
        first_visible_column: usize,
        /// An in-flight host-side scrollbar thumb drag (armed by a press on
        /// the thumb while the edit holds the mouse capture). `None` between
        /// interactions.
        scrollbar_drag: Option<ScrollDrag>,
        /// Caret blink phase: `true` draws the caret bar, `false` hides it.
        /// The focused EDIT toggles it on its internal WM_TIMER (F5 caret
        /// blink, ~530 ms — SPI_GETCARETTIMEOUT's default); paint draws the
        /// caret only in the on phase.
        caret_on: bool,
        /// The visual row where the last paint DREW the caret bar (`None`
        /// until a paint has drawn it). The surface keeps that bar until the
        /// row is repainted without it, so the caret-blink tick invalidates
        /// BOTH this row and the caret's current row — a caret that moved
        /// since the last paint must never leave the old bar behind (the
        /// stuck/ghost caret). Set only when the bar is actually drawn; a
        /// stale value (a paint skipped the bar) merely repaints an empty
        /// row once.
        last_caret_drawn_row: Option<usize>,
        /// Tab stop positions in dialog units (`EM_SETTABSTOPS`). Only the
        /// tests read the stored stops today; the typing/tab-expansion path
        /// consumes them in Task 2.3.
        tab_stops: Vec<u16>,
        /// The repaint scope for the next paint (see [`EditInvalidation`]):
        /// `Clean`/`Full` repaint every visible row, `Band` repaints only the
        /// dirty rows. Set by the mutating ops for the rows they touch,
        /// reset to `Full` by structural changes, and consumed (back to
        /// `Clean`) by `paint_edit`.
        invalid_rows: EditInvalidation,
        /// How many visual rows the last paint actually rendered (the
        /// row-level invalidation coverage counter; 0 before the first
        /// paint). Test-only observability: typing one char must paint ≤ the
        /// rows it changed, while a structural change still paints every
        /// visible row.
        last_paint_rows: usize,
    },
    /// LISTBOX (item list; the wheel and arrow keys scroll it).
    ListBox {
        /// List items, in insertion order.
        items: Vec<String>,
        /// Selected item index (-1 = no selection).
        sel_index: i32,
        /// First visible item index — the LISTBOX scroll offset. The mouse
        /// wheel moves it (3 rows per notch, the Windows default), the arrow
        /// keys keep the selection inside the viewport (scroll-into-view),
        /// and the paint renders the item rows from here. Scrollbar CHROME is
        /// deliberately deferred (the EDIT's precedent): the viewport scrolls
        /// with no visible scrollbar thumb until a later task.
        first_visible: usize,
        /// The repaint scope for the next paint (see [`LabelInvalidation`]) —
        /// the row-band equivalent of the BUTTON/STATIC scope: a scroll marks
        /// the union of the old+new visible bands, a selection change the old+
        /// new selected rows, an item append the new row. `paint_control`
        /// erases exactly the pending rect, so the published frame's `region`
        /// is the true changed area instead of the full control rect. The
        /// first paint (and every structural change — WM_SETFONT, a resize)
        /// stays `Full`.
        invalidation: LabelInvalidation,
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
    Static {
        /// The repaint scope for the next paint — the same
        /// [`LabelInvalidation`] the BUTTON variant carries: a caption
        /// change marks the text rect, the first paint covers everything.
        invalidation: LabelInvalidation,
    },
    /// STATUSCLASSNAMEW (status bar): the SB_* parts and per-part texts.
    ///
    /// The bar renders as a BTNFACE strip at the bottom (or top) of its
    /// parent with one text cell per part; the part list and texts arrive
    /// through `SB_SETPARTS` / `SB_SETTEXTW` (handled in `comctl32.rs`).
    StatusBar {
        /// Right-edge x of each part in the bar's client coords (the
        /// `SB_SETPARTS` array; a -1 entry means "extend to the right
        /// edge"). Empty = never configured — one part spans the whole
        /// width (and part 0 shows the `CreateStatusWindowA/W` text, which
        /// real comctl32 applies as `SB_SETTEXT(0, …)` internally).
        part_rights: Vec<i32>,
        /// Per-part text (`SB_SETTEXTW`), indexed by part; a part past the
        /// end of the vec renders empty. Part 0 falls back to the window's
        /// `control_text` (the creation text) until `SB_SETTEXTW(0, …)`
        /// overwrites it.
        part_texts: Vec<String>,
    },
}

/// An in-flight host-side scrollbar thumb drag (armed by a press on the
/// thumb while the edit holds the mouse capture). `vertical` picks the axis;
/// `grab_offset` is the pointer's offset from the thumb's leading edge in px,
/// so the thumb does not jump to the pointer on the first move.
#[derive(Debug, Clone, Copy)]
pub struct ScrollDrag {
    pub vertical: bool,
    pub grab_offset: i32,
}

/// The repaint scope of an EDIT between paints — what `paint_edit` must
/// redraw on the next `WM_PAINT` (the row-level invalidation: a caret blink
/// or a typed character repaints only the rows it touches instead of the
/// whole control).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditInvalidation {
    /// Nothing is pending — the control is clean since its last paint. The
    /// window's own `invalidated` flag still drives the next paint (which
    /// covers every visible row), but the next mutation band starts fresh.
    #[default]
    Clean,
    /// Exactly the visual rows `lo..=hi` are dirty — a caret blink, typing,
    /// or a selection change. `wrap_width` is the wrap column the range was
    /// computed against: a resize reflows the wrap, so a band computed at a
    /// different width is stale and the paint falls back to a full repaint.
    Band(EditInvalidRows),
    /// The whole client is dirty — the initial state and every structural
    /// change (a scroll move, `WM_SETFONT`, a whole-text replacement, a
    /// resize reflow). Sticky: a later mutation band must not narrow it.
    Full,
}

/// The visual-row band an EDIT must repaint (see [`EditInvalidation::Band`]).
/// Rows use the same global visual-row numbering `visual_rows` /
/// `layout_visible_lines` produce, so the clipped row loop and the scroll
/// math always agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditInvalidRows {
    /// First dirty visual row (inclusive).
    pub lo: usize,
    /// Last dirty visual row (inclusive).
    pub hi: usize,
    /// The wrap column the range was computed against — the staleness check:
    /// a pending band whose wrap width no longer matches the layout is
    /// ignored in favor of a full repaint (the rows reflowed underneath it).
    pub wrap_width: i32,
}

/// The repaint scope of a BUTTON/STATIC control between paints — what the
/// next paint must redraw (the label-control equivalent of
/// [`EditInvalidation`]'s row bands: a pressed-state change repaints only
/// the face rect, a text change only the caption rect, instead of the whole
/// control). The scope feeds the same B3 dirty-region machinery the EDIT's
/// band does: the paint erases exactly the pending rect, so the published
/// frame's `region` is the true changed area, not the full control rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelInvalidation {
    /// Nothing is pending — the control is clean since its last paint. The
    /// window's own `invalidated` flag still drives the next paint (which
    /// covers the whole control), but the next change starts fresh.
    #[default]
    Clean,
    /// Exactly this client-relative rect is dirty — a pressed-state change
    /// or a caption change. The rect is stamped with the control size it was
    /// computed against: a resize reflows the layout, so a rect at a
    /// different size is stale and the paint falls back to a full repaint.
    Rect(LabelInvalidRect),
    /// The whole client is dirty — the initial state and every structural
    /// change (a font change, a resize). Sticky: a later partial mark must
    /// not narrow it.
    Full,
}

/// A label control's pending dirty rect (see [`LabelInvalidation::Rect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelInvalidRect {
    /// First dirty client x (inclusive), right/bottom exclusive.
    pub rect: IRect,
    /// The control width the rect was computed against — the staleness
    /// check: a pending rect whose size no longer matches the layout is
    /// ignored in favor of a full repaint (the control reflowed underneath
    /// it).
    pub width: i32,
    /// The control height the rect was computed against.
    pub height: i32,
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
/// WndProc bridge for `WM_COMMAND`, or — for a guest-subclassed control — the
/// bridge to the subclass proc) or a real handler error.
pub(crate) fn dispatch_control_proc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    dispatch_control_proc_with_outer(
        engine,
        state,
        hwnd,
        message,
        word_parameter,
        long_parameter,
        OuterReturn::Passthrough,
    )
}

/// [`dispatch_control_proc`] with an explicit [`OuterReturn`] for the outer
/// host API that triggered the dispatch.
///
/// The focus-change pair (`deliver_focus_change`) passes its fixed
/// [`OuterReturn`] so a bridged subclass's LRESULT never replaces the outer
/// API's own result (SetFocus must report the previous focus window).
pub(crate) fn dispatch_control_proc_with_outer(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
    outer_return: OuterReturn,
) -> Result<Option<u64>> {
    if find_window(state, hwnd)
        .and_then(|w| w.control_kind)
        .is_none()
    {
        return Ok(None);
    }

    // Guest-subclass bridge: a control whose GWLP_WNDPROC the guest replaced
    // (notepad's EDIT_WndProc via SetWindowLongPtrW) sees EVERY message
    // through the subclass proc first. The subclass updates the title
    // star / status-bar caret and forwards what it does not handle through
    // CallWindowProcW(hwnd, <original marker>, …), which re-enters the host
    // default dispatch (see `dispatch_control_proc_host_default`). Without
    // this the subclass never runs and the star / Ln-Col behavior silently
    // disappears.
    let subclass = super::get_window_long_ptr_value(
        hwnd,
        super::GWLP_WNDPROC_RAW,
        state,
        "control subclass dispatch",
    )?;
    if subclass != 0 {
        let unicode = find_window(state, hwnd).is_some_and(|w| w.unicode);
        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: subclass,
                window_handle: hwnd,
                message,
                word_parameter,
                long_parameter,
                unicode,
                outer_return,
            },
        }
        .into());
    }

    dispatch_control_proc_host_default(engine, state, hwnd, message, word_parameter, long_parameter)
}

/// The host's DEFAULT control dispatch, ignoring any guest subclass.
///
/// This is what `CallWindowProcW(hwnd, <the original marker>, …)` runs when a
/// guest subclass forwards a message it does not handle: the subclass must
/// NOT intercept its own forwarded message, or the notepad flow would loop
/// subclass → CallWindowProc → subclass forever.
pub(crate) fn dispatch_control_proc_host_default(
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
        // An EDIT window's messages first ask the edit module whether it owns
        // them: the EDIT-specific arms (WM_CHAR, the EM_*, the caret-blink
        // timer, the edit mouse gestures, the focus pair, WM_GETDLGCODE) MUST
        // shadow the generic arms below — an Edit's WM_LBUTTONDOWN places the
        // caret, it does NOT fall through to the generic press arm, and the
        // EM_* messages have no generic arm at all. `Ok(None)` means the
        // message is not an edit message (WM_PAINT, WM_GETTEXT, WM_SETTEXT,
        // WM_SETFONT, WM_COMMAND, WM_LBUTTONUP, ...) — the generic arms run
        // unchanged.
        if self == ControlClassKind::Edit {
            // The EDIT-specific dispatch runs first (see the module comment on
            // `dispatch_edit_message`); an EDIT message it owns returns Some
            // and shadows the generic arms below.
            let handled = dispatch_edit_message(
                engine,
                state,
                hwnd,
                message,
                word_parameter,
                long_parameter,
            )?;
            if handled.is_some() {
                return Ok(handled);
            }
            // Ok(None): not an edit message — the generic arms run unchanged.
        }
        match (self, WinMsg::from(message)) {
            (_, WinMsg::WM_PAINT) => {
                // A hidden window must not paint: real Windows discards its
                // invalidated region (no WM_PAINT while hidden). The synthesized
                // paint can still reach a just-hidden control (synth.rs selects
                // invalidated windows without a visibility check), so the paint
                // is gated here — otherwise a hidden status bar keeps painting
                // over the control that grew into its space (View > Status Bar
                // regression). The invalidation is consumed either way, so the
                // hidden window cannot re-enter the paint cycle every idle
                // drain; ShowWindow(SW_SHOW) re-arms it, so the window repaints
                // the moment it is shown again. A hidden control's erase is
                // skipped with the paint (it lives inside paint_control).
                let visible = find_window(state, hwnd).is_some_and(|w| w.visible);
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.invalidated = false;
                }
                if visible {
                    paint_control(state, engine, hwnd, self)?;
                    // Publish the ancestor surface so the painted control
                    // becomes visible immediately — the ancestor's own WM_PAINT
                    // BitBlt may never run again (a modal dialog otherwise
                    // renders as an empty gray box until an unrelated repaint).
                    // Mirrors how paint_dialog publishes its face. B3.6:
                    // deferred — the runtime drains pending publishes once per
                    // repaint cycle (at the empty-queue idle boundary), so a
                    // repaint cycle (parent BitBlt + each child paint) emits
                    // one frame with the union region.
                    if let Some(ancestor) = resolve_window_ancestor(state, hwnd) {
                        state.present().publish_deferred(ancestor.hwnd);
                    }
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
                    // A click on an item row selects it and notifies the
                    // parent; the selection is kept visible (a click on a row
                    // below the fold scrolls it into view).
                    let clicked = listbox_hit_item(state, hwnd, long_parameter);
                    let previous = control_sel_index(state, hwnd);
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
                    listbox_scroll_selection_into_view(state, hwnd);
                    if changed {
                        // The old+new selected rows swap their highlight; a
                        // click never scrolls (the clicked row is on-screen),
                        // so the row marks are the whole scope.
                        listbox_invalidate_selection(state, hwnd, previous, clicked.unwrap_or(-1));
                    }
                    invalidate(state, hwnd);
                    if changed {
                        return listbox_notify_change(state, hwnd);
                    }
                    return Ok(Some(0));
                }
                // A push-button press changes the whole face (the pressed
                // color + the shifted caption); the other controls just mark
                // the window for a full repaint.
                if self == ControlClassKind::Button {
                    button_invalidate_pressed(state, hwnd);
                } else {
                    invalidate(state, hwnd);
                }
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
                        // The release restores the unpressed face — the same
                        // face-rect scope the press marked.
                        button_invalidate_pressed(state, hwnd);
                    } else {
                        invalidate(state, hwnd);
                    }
                    // BN_CLICKED is a BUTTON notification; other pressed
                    // controls (an EDIT mid-drag) must not command the parent.
                    if self == ControlClassKind::Button {
                        let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                        let command_wparam = make_command_wparam(id, BN_CLICKED);
                        return deliver_button_command(engine, state, hwnd, command_wparam);
                    }
                }
                // An EDIT's drag session ends on the button-up whether or not
                // the press was recorded on this window — a stray release must
                // not strand the mouse capture.
                if self == ControlClassKind::Edit {
                    edit_mouse_up(state, hwnd);
                    // A completed click navigation refreshes the parent's
                    // status-bar caret — the same EN_VSCROLL a caret move by
                    // keys delivers (RNotepad's EDIT subclass re-reads Ln/Col
                    // on it). Gated on a real press so a stray release stays
                    // silent.
                    if was_pressed {
                        return edit_notify_scroll(state, hwnd, EN_VSCROLL);
                    }
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
                    button_invalidate_pressed(state, hwnd);
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
                    button_invalidate_pressed(state, hwnd);
                    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
                    let command_wparam = make_command_wparam(id, BN_CLICKED);
                    return deliver_button_command(engine, state, hwnd, command_wparam);
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
                deliver_button_command(engine, state, hwnd, command_wparam)
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
                button_invalidate_pressed(state, hwnd);
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
            // WM_SETFONT / WM_GETFONT: DefWindowProc semantics shared by every
            // window kind — the font the control draws its text with is a
            // window property, not a per-kind control message. The stored
            // HFONT feeds the paint-path font resolution (task 2.7); notepad
            // sends WM_SETFONT to its EDIT right after creation.
            (_, WinMsg::WM_SETFONT) => {
                // A font change reflows every row: an EDIT's pending row band
                // is stale, and a label control's pending caption rect is
                // stale too (the ink moves with the new metrics). The reset
                // stays even when `redraw` is 0 — the next paint, whenever it
                // comes, must re-render with the new font.
                match find_window(state, hwnd).and_then(|w| w.control_kind) {
                    Some(ControlClassKind::Edit) => edit_reset_invalid_rows(state, hwnd),
                    // A font change reflows a LISTBOX's row pitch too (the row
                    // bands shift with the new line height).
                    Some(
                        ControlClassKind::Button
                        | ControlClassKind::Static
                        | ControlClassKind::ListBox,
                    ) => label_reset_invalid_full(state, hwnd),
                    _ => {}
                }
                super::window::set_window_font(state, hwnd, word_parameter, long_parameter);
                Ok(Some(0))
            }
            (_, WinMsg::WM_GETFONT) => Ok(Some(super::window::window_font(state, hwnd))),
            (_, WinMsg::WM_SETTEXT) => {
                if long_parameter == 0 {
                    return Ok(Some(0));
                }
                let text = if unicode {
                    read_guest_utf16_lossy(engine, long_parameter, 32_768)?
                } else {
                    read_guest_ansi_lossy(engine, long_parameter, 32_768)?
                };
                // A label control's old caption must survive the replacement:
                // the text-change invalidation measures BOTH the old and the
                // new caption rects so the next paint erases the previous
                // glyphs too.
                let kind = find_window(state, hwnd).and_then(|w| w.control_kind);
                let old_text = if matches!(
                    kind,
                    Some(ControlClassKind::Button | ControlClassKind::Static)
                ) {
                    find_window(state, hwnd).map_or_else(String::new, |w| w.control_text.clone())
                } else {
                    String::new()
                };
                if let Some(window) = find_window_mut(state, hwnd) {
                    window.control_text = text.clone();
                    window.invalidated = true;
                }
                // The text changed: an EDIT's cached EM_GETHANDLE buffer is
                // stale (the old handle is the guest's to LocalFree), and the
                // undo buffer is dropped — real Windows never lets WM_UNDO
                // revert past program-set text.
                edit_invalidate_text_buffer(state, hwnd);
                edit_clear_undo_buffer(state, hwnd);
                match kind {
                    // A whole-text replacement rewrites every row: reset any
                    // pending row band so the next paint covers the whole EDIT.
                    Some(ControlClassKind::Edit) => edit_reset_invalid_rows(state, hwnd),
                    // A label caption change narrows the next paint to the
                    // caption rect (the face/border are unchanged).
                    Some(ControlClassKind::Button | ControlClassKind::Static) => {
                        label_invalidate_text_change(state, hwnd, &old_text, &text)
                    }
                    _ => {}
                }
                Ok(Some(1))
            }
            (ControlClassKind::Button, WinMsg::WM_GETDLGCODE) => {
                let style = find_window(state, hwnd).map_or(0, |w| w.style);
                let push = if style & BS_DEFPUSHBUTTON != 0 {
                    DLGC_DEFPUSHBUTTON
                } else {
                    DLGC_UNDEFPUSHBUTTON
                };
                Ok(Some(DLGC_BUTTON | push))
            }
            // WM_MOUSEWHEEL: the signed delta in the wParam high word scrolls
            // the LISTBOX item viewport (3 rows per notch — the Windows
            // default). Same focus routing as the EDIT.
            (ControlClassKind::ListBox, WinMsg::WM_MOUSEWHEEL) => {
                if listbox_scroll_wheel(state, hwnd, word_parameter) {
                    invalidate(state, hwnd);
                }
                Ok(Some(0))
            }
            // Arrow keys on a focused LISTBOX move the selection one row and
            // keep it visible (scroll-into-view); a changed selection
            // notifies the parent like a click. The focus gate mirrors the
            // push-button space handling — keys only steer the focused
            // control.
            (ControlClassKind::ListBox, WinMsg::WM_KEYDOWN)
                if matches!(word_parameter & 0xFF, VK_UP | VK_DOWN) =>
            {
                if state.window_state().focus_window_handle == crate::handles::Hwnd::from(hwnd)
                    && listbox_key_move_selection(state, hwnd, word_parameter)
                {
                    invalidate(state, hwnd);
                    return listbox_notify_change(state, hwnd);
                }
                Ok(Some(0))
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
                // A programmatic selection lands on a possibly-scrolled
                // viewport: bring it into view like a click would (the scroll
                // marks its own band change when it moves).
                listbox_scroll_selection_into_view(state, hwnd);
                if previous != index {
                    // The old+new selected rows swap their highlight (a LISTBOX
                    // only; the ComboBox arm shares this dispatch and ignores
                    // the mark).
                    listbox_invalidate_selection(state, hwnd, previous, index);
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
                let index = {
                    let (ControlState::ListBox { items, .. }
                    | ControlState::ComboBox { items, .. }) = control_state_mut(state, hwnd)
                    else {
                        return Ok(Some(u64::MAX));
                    };
                    items.push(text);
                    u64::try_from(items.len().saturating_sub(1)).unwrap_or(u64::MAX)
                };
                // The appended row shows the new item (a LISTBOX only; the
                // ComboBox arm shares this dispatch and ignores the mark).
                listbox_invalidate_appended(state, hwnd);
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
            (_, WinMsg::WM_COMMAND) => deliver_button_command(engine, state, hwnd, word_parameter),
            // Status-bar messages are WM_USER+ offsets (commctrl.h) that
            // `WinMsg` cannot name, so the whole StatusBar kind dispatches
            // here on the raw value. The generic arms above (WM_PAINT,
            // WM_GETTEXT, WM_SETTEXT, WM_SETFONT, …) still run first — the
            // status bar inherits the shared control behaviors — and this
            // arm only sees what falls through (the SB_* messages plus
            // WM_SIZE, which repositions the bar in its parent).
            (ControlClassKind::StatusBar, _) => crate::comctl32::dispatch_status_bar_message(
                engine,
                state,
                hwnd,
                message,
                word_parameter,
                long_parameter,
            ),
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

/// Deliver a push-button `BN_CLICKED` command — or, when the button belongs
/// to a host-owned modeless dialog (comdlg32 Find/Replace), hand the command
/// to the host dialog instead of the guest: the handler writes the
/// `FINDREPLACE` struct back and posts `FINDMSGSTRING`.
///
/// Split from [`deliver_command`] because the host dialog needs the engine
/// for its guest-memory write-back, and `deliver_command` is also called from
/// engine-less sites (the EDIT `EN_*` notifications in `edit.rs`). The find
/// dialog's buttons are direct children, so only the direct parent is tested.
fn deliver_button_command(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    child_hwnd: u64,
    word_parameter: u64,
) -> Result<Option<u64>> {
    let parent_hwnd = find_window(state, child_hwnd).map_or(0, |w| w.parent_handle.as_u64());
    if parent_hwnd != 0 && crate::comdlg32::is_find_dialog_window(state, parent_hwnd) {
        return crate::comdlg32::handle_find_dialog_command(
            engine,
            state,
            parent_hwnd,
            word_parameter,
            child_hwnd,
        );
    }
    deliver_command(state, child_hwnd, word_parameter)
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
///
/// `kind.new_state()` seeds an EDIT with `style_bits = 0` (no window context
/// at the call site); re-capture the window's real creation style whenever
/// this path touches an EDIT so the ES_* flags survive whichever seeder runs
/// first (`edit_state_mut` refreshes the same field on its side).
fn control_state_mut(state: &mut WinApiState, hwnd: u64) -> &mut ControlState {
    let kind = find_window(state, hwnd)
        .and_then(|w| w.control_kind)
        .unwrap_or(ControlClassKind::Static);
    // Read the creation style before the entry borrow: the window record and
    // the control state live under the same `WindowState`.
    let style = if kind == ControlClassKind::Edit {
        find_window(state, hwnd).map_or(0, |w| w.style)
    } else {
        0
    };
    let control = state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| kind.new_state());
    if let ControlState::Edit { style_bits, .. } = control {
        *style_bits = style;
    }
    control
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
        use super::{ControlClassKind, ControlState, LabelInvalidation};
        assert!(matches!(
            ControlClassKind::Button.new_state(),
            ControlState::Button {
                default_push: false,
                invalidation: LabelInvalidation::Full,
                ..
            }
        ));
        assert!(matches!(
            ControlClassKind::Edit.new_state(),
            ControlState::Edit {
                caret: 0,
                sel_start: 0,
                sel_end: 0,
                ..
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
            ControlState::Static {
                invalidation: LabelInvalidation::Full,
                ..
            }
        ));
        assert!(matches!(
            ControlClassKind::StatusBar.new_state(),
            ControlState::StatusBar { .. }
        ));
    }
}
