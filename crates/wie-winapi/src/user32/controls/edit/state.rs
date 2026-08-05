//! EDIT constants and the shared state accessors — the plumbing every other
//! `edit` submodule builds on (the seeded `ControlState::Edit` accessors, the
//! text read, and the style/scrollbar predicates). Split from the monolithic
//! `edit.rs` along the state/math/mutation/messages/mouse/paint seams; the
//! `pub(super)` items here are the cross-file surface the sibling submodules
//! import through `super::state::…`.

use crate::user32::WinApiState;
use crate::user32::controls::{ControlClassKind, ControlState, ES_MULTILINE};

/// Cap for guest buffer reads (EM_SETHANDLE / EM_REPLACESEL adoption).
pub(super) const MAX_GUEST_TEXT: usize = 1 << 20;

/// The EDIT's internal caret-blink timer id — the id a real Windows EDIT
/// control uses for its caret timer, so a guest SetTimer on the same
/// window/id replaces it exactly like Windows (the timer is a plain record in
/// the thread's timer list).
pub(super) const CARET_TIMER_ID: u64 = 1;

/// Caret blink half-period in ms — `SPI_GETCARETTIMEOUT`'s 530 ms default
/// (the spec's ~530 ms).
pub(super) const CARET_BLINK_MS: u32 = 530;

/// `WS_HSCROLL` — a multiline EDIT with a horizontal scrollbar does NOT word
/// wrap (notepad toggles wrap by dropping the horizontal scroll style).
pub(super) const WS_HSCROLL: u32 = 0x0010_0000;

/// `WS_VSCROLL` — a multiline EDIT requests a vertical scrollbar. The chrome
/// only shows when the content ALSO overflows the viewport (`scrollbar_visible`
/// gates on both); an EDIT without the style never reserves the gutter.
const WS_VSCROLL: u32 = 0x0020_0000;

/// Whether an EDIT shows its vertical scrollbar: the window carries the
/// `WS_VSCROLL` style AND the content (`total` visual rows) overflows the
/// viewport (`visible` rows). Auto-hides when the content fits.
#[must_use]
pub(crate) fn scrollbar_visible(style: u32, total: usize, visible: usize) -> bool {
    style & WS_VSCROLL != 0 && total > visible
}

/// Whether an EDIT word-wraps: multiline AND no horizontal scrollbar (notepad
/// toggles wrap by dropping the horizontal scroll style); long lines are
/// horizontally clipped otherwise. Single source of truth for the paint, the
/// scroll math, and the click hit-test.
#[must_use]
pub(super) fn edit_wrap_from_style(style: u32) -> bool {
    style & ES_MULTILINE != 0 && style & WS_HSCROLL == 0
}

/// The classic scrollbar gutter — `SM_CXVSCROLL` (17 px). A multiline EDIT
/// with an overflowing vertical scrollbar reserves this strip in the right of
/// its client (shrinking the wrap column); the horizontal scrollbar reserves
/// the same strip at the bottom.
pub(super) const SCROLLBAR_WIDTH: i32 = 17;

/// One horizontal "line" scroll step in px (a nominal character cell; the
/// host-side H scrollbar has no per-glyph metric at the message boundary).
pub(super) const H_LINE_STEP: usize = 8;

/// The ES_LEFT/CENTER/RIGHT alignment bits (the low 2 style bits).
pub(super) const ES_ALIGN_MASK: u32 = 0x0003;

/// The EDIT control's state, seeded on demand. Only reachable from the
/// `(Edit, _)` dispatch arms, so the seed kind is always `Edit`. Takes the
/// `control_states` field (not the whole `WindowState`) so callers can hold a
/// `window` borrow from `ws.windows` at the same time (disjoint fields). The
/// seed captures the window's creation style into `style_bits` — and RE-captures
/// it on every touch, because a window's creation style never changes, so the
/// refresh is a no-op for a correct seed while healing a state that was first
/// seeded with style 0 by a different seeder (`control_state_mut`'s
/// `kind.new_state()` runs before this path saw the window record).
pub(super) fn edit_state_mut(
    control_states: &mut ahash::HashMap<crate::handles::Hwnd, ControlState>,
    hwnd: u64,
    style: u32,
) -> &mut ControlState {
    let state = control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::new_edit_state(style));
    if let ControlState::Edit { style_bits, .. } = state {
        *style_bits = style;
    }
    state
}

/// The EDIT state for `hwnd`, seeded with the window's creation style when
/// first touched. Centralizes the style lookup so the EM_* setters that do not
/// otherwise hold the window record read it exactly once.
pub(super) fn edit_state_for_window(state: &mut WinApiState, hwnd: u64) -> &mut ControlState {
    let ws = state.window_state();
    let style = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map_or(0, |w| w.style);
    edit_state_mut(&mut ws.control_states, hwnd, style)
}

/// The control text of `hwnd`, when it is a known window.
pub(super) fn edit_text(state: &WinApiState, hwnd: u64) -> Option<&str> {
    state.try_window_state().and_then(|ws| {
        ws.windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map(|w| w.control_text.as_str())
    })
}
