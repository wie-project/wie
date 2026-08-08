//! LISTBOX/COMBOBOX item logic (selection notifications, hit-testing,
//! viewport scrolling) and the shared text-rendering helper (split from
//! `controls.rs`).

use anyhow::Result;

use super::button::invalidate_control_rect;
use super::paint::fill_rect_clipped;
use super::{
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, ControlClassKind, ControlState, Dimension, PaintCtx,
    PaintFont, WHEEL_DELTA, WHEEL_SCROLL_LINES, control_items, control_sel_index, control_state,
    control_state_mut, deliver_command,
};
use crate::gdi32::render_text_into_surface;
use crate::gdi32::{IRect, ResolvedWindow};
use crate::user32::{LBN_SELCHANGE, VK_DOWN, VK_UP, WinApiState, find_window, make_command_wparam};

/// Draw the LISTBOX items (one line each) from the viewport's first-visible
/// index, filling the selected item's row with COLOR_HIGHLIGHT and rendering
/// its glyphs in COLOR_HIGHLIGHTTEXT (Windows' selected-listbox look). The
/// scroll offset is read from the control state via the DC's own hwnd
/// (`resolve_window_ancestor` sets `dc_window` to the painting control), so
/// the caller does not need to slice the item list.
///
/// `dirty` is the pending invalidation rect (client-relative): only rows
/// overlapping it render. The erase covered exactly that band, so the
/// untouched rows keep their pixels — a partial repaint must not wipe them,
/// and the glyph bands of the rendered rows all land inside the erased area
/// (the region the published frame reports stays the true changed band).
pub(super) fn paint_item_lines(
    ctx: &mut PaintCtx<'_>,
    info: &ResolvedWindow,
    items: &[String],
    area: Dimension,
    sel_index: i32,
    font: &mut PaintFont<'_>,
    dirty: IRect,
) -> Result<()> {
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(area.width);
    let bottom = y.saturating_add(area.height);
    let line_h = font.resolved.line_height();
    // The dirty band in surface coords — the row-overlap test against it.
    let dirty_top = y.saturating_add(dirty.top);
    let dirty_bottom = y.saturating_add(dirty.bottom);
    let first_visible = control_state(ctx.state, info.dc_window.as_u64())
        .and_then(|state| match state {
            ControlState::ListBox { first_visible, .. } => Some(*first_visible),
            _ => None,
        })
        .unwrap_or(0);
    for (index, item) in items.iter().enumerate().skip(first_visible) {
        // The on-screen row is the item index minus the scroll offset.
        let row = index.saturating_sub(first_visible);
        let line_y = y.saturating_add(i32::try_from(row).unwrap_or(0).saturating_mul(line_h));
        if line_y >= bottom {
            break;
        }
        if line_y.saturating_add(line_h) <= dirty_top || line_y >= dirty_bottom {
            // Outside the pending band: the row was not erased, so its pixels
            // are intact and rendering it would widen the region.
            continue;
        }
        let selected = sel_index >= 0 && i32::try_from(index).unwrap_or(-1) == sel_index;
        if selected {
            fill_rect_clipped(
                ctx.state,
                info,
                area,
                IRect::from_xywh(x, line_y, area.width, line_h),
                COLOR_HIGHLIGHT,
            );
        }
        let color = if selected { COLOR_HIGHLIGHTTEXT } else { 0 };
        render_control_text(
            ctx,
            info.hwnd,
            IRect::from_xywh(
                x.saturating_add(2),
                line_y,
                i32::try_from(info.width).unwrap_or(0),
                i32::try_from(info.height).unwrap_or(0),
            ),
            item,
            color,
            Some(IRect {
                left: x,
                top: y,
                right,
                bottom,
            }),
            font,
        )?;
    }
    Ok(())
}

/// Render control text into the ancestor surface (TRANSPARENT background).
pub(super) fn render_control_text(
    ctx: &mut PaintCtx<'_>,
    top_hwnd: crate::handles::Hwnd,
    rect: IRect,
    text: &str,
    color: u32,
    clip: Option<IRect>,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    ctx.state.present().ensure_surface(
        top_hwnd,
        u32::try_from(rect.width()).unwrap_or(0),
        u32::try_from(rect.height()).unwrap_or(0),
    );
    let Some(surface) = ctx.state.present().surfaces.get_mut(&top_hwnd) else {
        return Ok(());
    };
    render_text_into_surface(
        ctx.engine,
        font.engine,
        &mut surface.pixels,
        surface.width,
        surface.height,
        rect.left,
        rect.top,
        text,
        color,
        // The rasterizer's clip is a plain (l,t,r,b) tuple; the control paint
        // path reasons in `IRect`s, so the bundle unwraps at the boundary.
        clip.map(|c| (c.left, c.top, c.right, c.bottom)),
        font.resolved,
        font.key,
    )
}

/// Send `LBN_SELCHANGE` as WM_COMMAND(MAKEWPARAM(id, LBN_SELCHANGE)) to the
/// parent — the LISTBOX selection changed.
pub(super) fn listbox_notify_change(state: &mut WinApiState, hwnd: u64) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = make_command_wparam(id, LBN_SELCHANGE);
    deliver_command(state, hwnd, command_wparam)
}

/// Which LISTBOX item a client-relative click (packed `lParam`) falls on.
/// `None` when the click is outside the item rows or the list is empty.
///
/// The visible row is offset by the scroll position (`first_visible`), so a
/// click on a row of a scrolled listbox resolves to the item the paint shows
/// there.
#[must_use]
pub(super) fn listbox_hit_item(
    state: &mut WinApiState,
    hwnd: u64,
    long_parameter: u64,
) -> Option<i32> {
    let y_raw = u16::try_from((long_parameter >> 16) & 0xFFFF).unwrap_or(0);
    let y = i32::from(i16::from_ne_bytes(y_raw.to_ne_bytes()));
    let (count, first_visible) = match control_state(state, hwnd) {
        Some(ControlState::ListBox {
            items,
            first_visible,
            ..
        }) => (items.len(), *first_visible),
        _ => return None,
    };
    if y < 0 || count == 0 {
        return None;
    }
    // One line per item at the resolved font line height (the paint's row
    // pitch), flush at the control's top edge.
    let line_h = listbox_line_height(state, hwnd);
    let row = if line_h <= 0 {
        0
    } else {
        y.saturating_div(line_h)
    };
    let index = first_visible.saturating_add(usize::try_from(row).unwrap_or(0));
    if index >= count {
        return None;
    }
    Some(i32::try_from(index).unwrap_or(-1))
}

// ── Viewport scrolling ───────────────────────────────────────────────────
//
// The LISTBOX keeps a `first_visible` item index in its control state (like
// the EDIT's `first_visible_line`); the paint renders from it, the wheel
// moves it, and the arrow keys / clicks keep the selection inside the
// viewport. Scrollbar CHROME is deferred (the EDIT's precedent): the viewport
// scrolls with no visible scrollbar thumb until a later task.

/// The resolved line height (px) the LISTBOX paint + scroll math use — the
/// stored control font's line height, falling back to the 16 px system
/// default (the same resolution the paint path uses, so the scroll clamp and
/// the painted rows always agree).
fn listbox_line_height(state: &mut WinApiState, hwnd: u64) -> i32 {
    state.with_font_engine(|state, font_engine| {
        crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine)
            .map_or(16, |(_key, resolved)| resolved.line_height())
    })
}

/// How many item rows fit in the LISTBOX client at the resolved line height
/// (floor division — a partial row at the bottom is clipped). A degenerate
/// (zero-height) client still reports one row so the scroll clamp never
/// over-scrolls.
fn listbox_visible_rows(state: &mut WinApiState, hwnd: u64) -> usize {
    let height = find_window(state, hwnd).map_or(0, |w| w.height);
    let line_h = listbox_line_height(state, hwnd);
    if line_h <= 0 {
        return 1;
    }
    usize::try_from(height.saturating_div(line_h))
        .unwrap_or(0)
        .max(1)
}

/// Clamp a LISTBOX scroll offset so the viewport stays inside the item list:
/// `first_visible` at most `items.len() − visible_rows` (0 when everything
/// fits) — scrolling past the last row would leave a gap at the bottom.
fn listbox_clamp_first_visible(state: &mut WinApiState, hwnd: u64, first_visible: usize) -> usize {
    let item_count = control_items(state, hwnd).len();
    let visible = listbox_visible_rows(state, hwnd);
    first_visible.min(item_count.saturating_sub(visible))
}

/// LISTBOX: WM_MOUSEWHEEL — scroll the item viewport vertically. `wparam`'s
/// high word is the signed wheel delta (a wheel notch = 120 delta units); one
/// notch scrolls 3 rows — the Windows default (`SPI_GETWHEELSCROLLLINES`) —
/// and smaller trackpad deltas scroll proportionally. A positive delta (wheel
/// away from the user) scrolls UP. Returns whether the offset moved.
pub(crate) fn listbox_scroll_wheel(state: &mut WinApiState, hwnd: u64, wparam: u64) -> bool {
    let hi = u16::try_from((wparam >> 16) & 0xFFFF).unwrap_or(0);
    let delta = i32::from(i16::from_le_bytes(hi.to_le_bytes()));
    // Truncating division drops partial notches: a 60-unit trackpad flick is
    // a no-op while a full notch scrolls `WHEEL_SCROLL_LINES` rows.
    let rows = i64::from(
        delta
            .saturating_div(WHEEL_DELTA)
            .saturating_mul(WHEEL_SCROLL_LINES),
    );
    let old = match control_state(state, hwnd) {
        Some(ControlState::ListBox { first_visible, .. }) => *first_visible,
        _ => return false,
    };
    // Positive delta = wheel away from the user = scroll UP (earlier rows);
    // negative = toward the user = scroll DOWN (later rows).
    let target = if rows > 0 {
        old.saturating_sub(usize::try_from(rows).unwrap_or(0))
    } else {
        old.saturating_add(usize::try_from(rows.saturating_neg()).unwrap_or(0))
    };
    let clamped = listbox_clamp_first_visible(state, hwnd, target);
    let moved = {
        let ControlState::ListBox { first_visible, .. } = control_state_mut(state, hwnd) else {
            return false;
        };
        if *first_visible == clamped {
            false
        } else {
            *first_visible = clamped;
            true
        }
    };
    if moved {
        // The visible row band needs repainting (the scrolled-away rows must
        // be erased, the newly-exposed rows painted).
        listbox_invalidate_scroll(state, hwnd);
    }
    moved
}

/// Adjust the LISTBOX's `first_visible` by the smallest scroll that brings
/// the selected row fully into view (the arrow-key and click behavior). No-op
/// when the selection is absent or already visible. Returns whether the
/// offset moved.
pub(crate) fn listbox_scroll_selection_into_view(state: &mut WinApiState, hwnd: u64) -> bool {
    let (first_visible, sel_index, item_count) = match control_state(state, hwnd) {
        Some(ControlState::ListBox {
            items,
            sel_index,
            first_visible,
            ..
        }) => (*first_visible, *sel_index, items.len()),
        _ => return false,
    };
    if sel_index < 0 || usize::try_from(sel_index).unwrap_or(usize::MAX) >= item_count {
        return false;
    }
    let sel = usize::try_from(sel_index).unwrap_or(0);
    let visible = listbox_visible_rows(state, hwnd);
    let target = if sel < first_visible {
        // Above the viewport: make it the first visible row.
        sel
    } else if sel >= first_visible.saturating_add(visible) {
        // Below the last visible row: scroll so it becomes the last one.
        sel.saturating_sub(visible).saturating_add(1)
    } else {
        first_visible
    };
    let clamped = listbox_clamp_first_visible(state, hwnd, target);
    let moved = {
        let ControlState::ListBox { first_visible, .. } = control_state_mut(state, hwnd) else {
            return false;
        };
        if *first_visible == clamped {
            false
        } else {
            *first_visible = clamped;
            true
        }
    };
    if moved {
        // The visible row band needs repainting (the scrolled-away rows must
        // be erased, the newly-exposed rows painted).
        listbox_invalidate_scroll(state, hwnd);
    }
    moved
}

/// LISTBOX: WM_KEYDOWN for the Up/Down arrow keys — move the selection one
/// row (clamped to the item range) and keep it visible. Returns whether the
/// selection changed (the caller delivers LBN_SELCHANGE on a change, like a
/// click).
pub(crate) fn listbox_key_move_selection(state: &mut WinApiState, hwnd: u64, vk: u64) -> bool {
    let item_count = control_items(state, hwnd).len();
    if item_count == 0 {
        return false;
    }
    let old = control_sel_index(state, hwnd);
    let next = match vk & 0xFF {
        VK_UP => {
            if old <= 0 {
                return false;
            }
            old.saturating_sub(1)
        }
        VK_DOWN => {
            let last = i32::try_from(item_count.saturating_sub(1)).unwrap_or(0);
            if old >= last {
                return false;
            }
            old.saturating_add(1)
        }
        _ => return false,
    };
    let changed = {
        let ControlState::ListBox { sel_index, .. } = control_state_mut(state, hwnd) else {
            return false;
        };
        if *sel_index == next {
            false
        } else {
            *sel_index = next;
            true
        }
    };
    listbox_scroll_selection_into_view(state, hwnd);
    if changed {
        // The old and new selected rows swap their highlight. A scroll (the
        // call above) marks its own band change when it moved; the row marks
        // cover the no-scroll case exactly.
        listbox_invalidate_selection(state, hwnd, old, next);
    }
    changed
}

// ── Row-level repaint invalidation (the LISTBOX half of fix-24) ──────────
//
// Mirrors the BUTTON/STATIC `LabelInvalidation` scope: the mutating ops mark
// the sub-band they change (a scroll marks the viewport band, a selection
// change the old+new selected rows, an item append the new row) and
// `paint_control` erases exactly the pending rect — the erase fill feeds the
// B3 dirty-region accumulator, so the published frame's `region` is the true
// changed area instead of the full control rect. The first paint (and every
// structural change — WM_SETFONT, a resize) stays full.

/// The client-relative rect of the LISTBOX's visible row band — screen rows
/// `0..visible` at the resolved line height, spanning the client's full width,
/// clamped to its height.
///
/// The viewport is a FIXED screen rect regardless of the scroll offset
/// (`first_visible` only selects which items fill it), so a scroll's old and
/// new bands coincide — the union of the two is this single band. Every
/// screen row's content shifts on a scroll (item `first_visible + r` replaces
/// `first_visible_old + r`), so marking this band once covers the whole
/// changed area.
fn listbox_band_rect(state: &mut WinApiState, hwnd: u64) -> IRect {
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    let line_h = listbox_line_height(state, hwnd);
    let visible = listbox_visible_rows(state, hwnd);
    let band_h = i32::try_from(visible)
        .unwrap_or(0)
        .saturating_mul(line_h)
        .min(height);
    IRect::from_xywh(0, 0, width.max(0), band_h.max(0))
}

/// The client-relative rect of item `index`'s on-screen row under the current
/// scroll offset — `None` when the row lies entirely off the client (a
/// selection above the viewport or past the bottom edge has no pixels to
/// repaint, and its visible predecessor is covered by the scroll's band mark).
fn listbox_row_rect(state: &mut WinApiState, hwnd: u64, index: i32) -> Option<IRect> {
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    let first_visible = match control_state(state, hwnd) {
        Some(ControlState::ListBox { first_visible, .. }) => *first_visible,
        _ => return None,
    };
    let line_h = listbox_line_height(state, hwnd);
    if line_h <= 0 {
        return None;
    }
    let row = index.saturating_sub(i32::try_from(first_visible).unwrap_or(0));
    if row < 0 {
        return None;
    }
    let top = row.saturating_mul(line_h);
    if top >= height {
        return None;
    }
    Some(IRect::from_xywh(
        0,
        top,
        width.max(0),
        line_h.min(height.saturating_sub(top)).max(0),
    ))
}

/// Mark a viewport scroll's repaint scope: the visible row band (the union of
/// the old and new screen bands, which coincide — see [`listbox_band_rect`]).
fn listbox_invalidate_scroll(state: &mut WinApiState, hwnd: u64) {
    let band = listbox_band_rect(state, hwnd);
    invalidate_control_rect(state, hwnd, band);
}

/// Mark a selection change's repaint scope: the old and new selected rows'
/// rects (their highlight swaps). No-op for a LISTBOX-less window (the
/// ComboBox shares the SETCURSEL/ADDSTRING dispatch) and for negative indices
/// (no previous selection to erase / a cleared selection paints nothing).
pub(super) fn listbox_invalidate_selection(
    state: &mut WinApiState,
    hwnd: u64,
    old_index: i32,
    new_index: i32,
) {
    if find_window(state, hwnd).and_then(|w| w.control_kind) != Some(ControlClassKind::ListBox) {
        return;
    }
    for index in [old_index, new_index] {
        if let Some(rect) = listbox_row_rect(state, hwnd, index) {
            invalidate_control_rect(state, hwnd, rect);
        }
    }
}

/// Mark the row an `LB_ADDSTRING` appended item lands on (the new item's
/// on-screen row now shows text where nothing was painted). No-op when the
/// row is off the viewport or the window is not a LISTBOX (the ComboBox
/// shares the ADDSTRING dispatch).
pub(super) fn listbox_invalidate_appended(state: &mut WinApiState, hwnd: u64) {
    if find_window(state, hwnd).and_then(|w| w.control_kind) != Some(ControlClassKind::ListBox) {
        return;
    }
    let count = match control_state(state, hwnd) {
        Some(ControlState::ListBox { items, .. }) => items.len(),
        _ => return,
    };
    if count == 0 {
        return;
    }
    if let Some(rect) = listbox_row_rect(
        state,
        hwnd,
        i32::try_from(count.saturating_sub(1)).unwrap_or(-1),
    ) {
        invalidate_control_rect(state, hwnd, rect);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        listbox_band_rect, listbox_hit_item, listbox_invalidate_appended,
        listbox_key_move_selection, listbox_line_height, listbox_row_rect,
        listbox_scroll_selection_into_view, listbox_scroll_wheel, listbox_visible_rows,
    };
    use crate::gdi32::IRect;
    use crate::guest_heap::GuestHeap;
    use crate::handles::Hwnd;
    use crate::present::MessageQueue;
    use crate::state::{FileIoState, HeapState, ProcessState};
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::controls::{
        ControlClassKind, ControlState, LabelInvalidRect, LabelInvalidation,
    };
    use crate::user32::{
        CreateWindowRequest, WS_CHILD, WS_VISIBLE, WindowClassIdentifier, create_window_record,
    };
    use crate::vfs::VolumeConfig;
    use crate::{DEFAULT_ENVIRONMENT, DllStateMap, KernelState, ModuleState, WinApiState};
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};

    /// A minimal `WinApiState` for the scroll-math unit tests (window records,
    /// control states, and a resolvable default font are all that is needed).
    fn test_state() -> WinApiState {
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    const VK_UP: u64 = 0x26;
    const VK_DOWN: u64 = 0x28;

    /// A LISTBOX window whose client height fits fewer rows than the seeded
    /// item count, so the wheel/arrow scroll math has room to move.
    fn listbox_window(state: &mut crate::WinApiState, height: i32) -> u64 {
        let (hwnd, _, _) = create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Atom(0x0083),
                title: String::new(),
                style: WS_CHILD | WS_VISIBLE,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 1,
                instance_handle: 0,
                x: 0,
                y: 0,
                width: 100,
                height,
            },
            false,
        )
        .expect("listbox window created");
        hwnd
    }

    fn seed_listbox(state: &mut crate::WinApiState, hwnd: u64, count: usize) {
        let ControlState::ListBox {
            items,
            sel_index,
            first_visible,
            invalidation,
            ..
        } = state
            .window_state()
            .control_states
            .entry(Hwnd::from(hwnd))
            .or_insert_with(|| ControlClassKind::ListBox.new_state())
        else {
            panic!("listbox state");
        };
        *items = (0..count).map(|i| format!("item {i}")).collect();
        *sel_index = 0;
        *first_visible = 0;
        // The seed simulates a painted control: the marks the tests drive
        // must be observable as a pending rect, not absorbed into the Full
        // a fresh control starts with.
        *invalidation = LabelInvalidation::Clean;
    }

    fn first_visible(state: &mut crate::WinApiState, hwnd: u64) -> usize {
        let ControlState::ListBox { first_visible, .. } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(hwnd))
            .expect("listbox state")
        else {
            panic!("listbox state");
        };
        *first_visible
    }

    fn sel_index(state: &mut crate::WinApiState, hwnd: u64) -> i32 {
        let ControlState::ListBox { sel_index, .. } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(hwnd))
            .expect("listbox state")
        else {
            panic!("listbox state");
        };
        *sel_index
    }

    /// A WM_MOUSEWHEEL wParam with the given signed wheel delta in the high
    /// word (a wheel notch = 120).
    fn wheel(delta: i32) -> u64 {
        u64::from(u16::from_ne_bytes(
            delta.to_ne_bytes()[..2].try_into().expect("delta"),
        )) << 16
    }

    #[test]
    fn wheel_scrolls_three_rows_per_notch_and_clamps() {
        let mut state = test_state();
        let hwnd = listbox_window(&mut state, 64);
        seed_listbox(&mut state, hwnd, 10);
        let visible = listbox_visible_rows(&mut state, hwnd);
        assert!(
            (1..10).contains(&visible),
            "the test listbox must not fit all items"
        );

        // One notch down scrolls 3 rows (unclamped: 10 - visible > 3).
        assert!(listbox_scroll_wheel(&mut state, hwnd, wheel(-120)));
        assert_eq!(first_visible(&mut state, hwnd), 3);
        assert!(listbox_scroll_wheel(&mut state, hwnd, wheel(-120)));
        assert_eq!(first_visible(&mut state, hwnd), 6);

        // The bottom clamp: the last visible row must stay on screen.
        for _ in 0..10 {
            listbox_scroll_wheel(&mut state, hwnd, wheel(-120));
        }
        assert_eq!(
            first_visible(&mut state, hwnd),
            10 - visible,
            "scrolling past the last row must clamp to items.len() - visible"
        );
        assert!(
            !listbox_scroll_wheel(&mut state, hwnd, wheel(-120)),
            "an already-clamped wheel-down is a no-op"
        );

        // Wheel up scrolls back toward the top; repeated notches return to 0.
        assert!(listbox_scroll_wheel(&mut state, hwnd, wheel(120)));
        assert_eq!(
            first_visible(&mut state, hwnd),
            (10 - visible).saturating_sub(3)
        );
        for _ in 0..10 {
            listbox_scroll_wheel(&mut state, hwnd, wheel(120));
        }
        assert_eq!(first_visible(&mut state, hwnd), 0);
    }

    #[test]
    fn arrow_keys_move_selection_and_keep_it_visible() {
        let mut state = test_state();
        let hwnd = listbox_window(&mut state, 64);
        seed_listbox(&mut state, hwnd, 10);
        let visible = listbox_visible_rows(&mut state, hwnd);
        assert!(visible < 10);

        // Arrow keys move the selection one row at a time.
        assert!(listbox_key_move_selection(&mut state, hwnd, VK_DOWN));
        assert_eq!(sel_index(&mut state, hwnd), 1);
        // VK_UP moves back to the top; at the top it is a no-op (no wrap).
        assert!(listbox_key_move_selection(&mut state, hwnd, VK_UP));
        assert_eq!(sel_index(&mut state, hwnd), 0);
        assert!(!listbox_key_move_selection(&mut state, hwnd, VK_UP));
        assert_eq!(sel_index(&mut state, hwnd), 0);

        // Jump the selection deep past the viewport, then scroll it into view.
        {
            let ControlState::ListBox { sel_index, .. } = state
                .window_state()
                .control_states
                .get_mut(&Hwnd::from(hwnd))
                .expect("listbox state")
            else {
                panic!("listbox state");
            };
            *sel_index = 8;
        }
        assert!(listbox_scroll_selection_into_view(&mut state, hwnd));
        let first = first_visible(&mut state, hwnd);
        assert!(
            8 >= first && 8 < first.saturating_add(visible),
            "the deep selection must be scrolled fully into view (first_visible={first}, visible={visible})"
        );

        // VK_DOWN from the deep selection keeps the new row visible.
        assert!(listbox_key_move_selection(&mut state, hwnd, VK_DOWN));
        assert_eq!(sel_index(&mut state, hwnd), 9);
        let first = first_visible(&mut state, hwnd);
        assert!(
            9 >= first && 9 < first.saturating_add(visible),
            "the moved selection must stay visible (first_visible={first}, visible={visible})"
        );
        // At the last item, VK_DOWN is a no-op.
        assert!(!listbox_key_move_selection(&mut state, hwnd, VK_DOWN));
    }

    #[test]
    fn click_hit_test_accounts_for_the_scroll_offset() {
        let mut state = test_state();
        let hwnd = listbox_window(&mut state, 200);
        seed_listbox(&mut state, hwnd, 10);
        {
            let ControlState::ListBox { first_visible, .. } = state
                .window_state()
                .control_states
                .get_mut(&Hwnd::from(hwnd))
                .expect("listbox state")
            else {
                panic!("listbox state");
            };
            *first_visible = 3;
        }
        let line_h = listbox_line_height(&mut state, hwnd);
        let lparam = |y: i32| u64::from(u16::try_from(y).unwrap_or(0)) << 16;
        // Row 0 of the viewport is item 3; row 1 is item 4.
        assert_eq!(listbox_hit_item(&mut state, hwnd, lparam(0)), Some(3));
        assert_eq!(listbox_hit_item(&mut state, hwnd, lparam(line_h)), Some(4));
        // A click past the last item resolves to nothing.
        assert_eq!(
            listbox_hit_item(&mut state, hwnd, lparam(line_h * 20)),
            None
        );
    }

    fn pending_invalidation(state: &mut crate::WinApiState, hwnd: u64) -> LabelInvalidation {
        let ControlState::ListBox { invalidation, .. } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(hwnd))
            .expect("listbox state")
        else {
            panic!("listbox state");
        };
        *invalidation
    }

    /// A 100-tall LISTBOX (the resolved line height leaves a partial band — a
    /// 100 × 100 client with a 16 px row pitch shows 6 rows in 96 px), so a
    /// scroll's viewport band is a proper sub-rect of the client.
    fn tall_listbox_window(state: &mut crate::WinApiState) -> u64 {
        listbox_window(state, 100)
    }

    /// A wheel scroll marks the viewport band — rows `0..visible` — as the
    /// pending repaint scope: the scrolled-away rows must be erased and the
    /// newly-exposed rows painted, and the screen band is the same rect
    /// before and after the scroll.
    #[test]
    fn wheel_scroll_marks_the_viewport_band() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 10);
        let line_h = listbox_line_height(&mut state, hwnd);
        let visible = listbox_visible_rows(&mut state, hwnd);

        assert!(listbox_scroll_wheel(&mut state, hwnd, wheel(-120)));
        let band_h = i32::try_from(visible)
            .unwrap_or(0)
            .saturating_mul(line_h)
            .min(100);
        assert_eq!(
            pending_invalidation(&mut state, hwnd),
            LabelInvalidation::Rect(LabelInvalidRect {
                rect: IRect::from_xywh(0, 0, 100, band_h),
                width: 100,
                height: 100,
            }),
            "a wheel scroll must mark exactly the visible row band"
        );
    }

    /// The arrow keys mark the old and new selected rows — the two-row union
    /// the next paint erases (the highlight swaps between them).
    #[test]
    fn key_move_marks_old_and_new_selected_rows() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 10);
        let line_h = listbox_line_height(&mut state, hwnd);

        assert!(listbox_key_move_selection(&mut state, hwnd, VK_DOWN));
        assert_eq!(
            pending_invalidation(&mut state, hwnd),
            LabelInvalidation::Rect(LabelInvalidRect {
                rect: IRect::from_xywh(0, 0, 100, line_h.saturating_mul(2)),
                width: 100,
                height: 100,
            }),
            "moving the selection one row marks rows 0 and 1 (no scroll)"
        );
    }

    /// A scroll mark followed by a selection mark accumulates into ONE pending
    /// scope — the band contains the rows, so the union stays the band (the
    /// same union semantics the BUTTON/STATIC marks share).
    #[test]
    fn scroll_then_selection_marks_union() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 10);
        let line_h = listbox_line_height(&mut state, hwnd);
        let visible = listbox_visible_rows(&mut state, hwnd);

        assert!(listbox_scroll_wheel(&mut state, hwnd, wheel(-120)));
        // Selecting a row inside the scrolled viewport (first_visible 3, so
        // item 4 is screen row 1) marks rows 4 and 5 — inside the band.
        assert!(listbox_key_move_selection(&mut state, hwnd, VK_DOWN));
        let band_h = i32::try_from(visible)
            .unwrap_or(0)
            .saturating_mul(line_h)
            .min(100);
        assert_eq!(
            pending_invalidation(&mut state, hwnd),
            LabelInvalidation::Rect(LabelInvalidRect {
                rect: IRect::from_xywh(0, 0, 100, band_h),
                width: 100,
                height: 100,
            }),
            "a selection mark inside the scrolled band must not widen it"
        );
    }

    /// An LB_ADDSTRING marks the appended item's on-screen row — the row now
    /// shows text where nothing was painted. An item below the viewport marks
    /// nothing.
    #[test]
    fn appended_item_marks_its_row() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 4);
        let line_h = listbox_line_height(&mut state, hwnd);

        {
            let ControlState::ListBox { items, .. } = state
                .window_state()
                .control_states
                .get_mut(&Hwnd::from(hwnd))
                .expect("listbox state")
            else {
                panic!("listbox state");
            };
            items.push("item 4".to_owned());
        }
        listbox_invalidate_appended(&mut state, hwnd);
        assert_eq!(
            pending_invalidation(&mut state, hwnd),
            LabelInvalidation::Rect(LabelInvalidRect {
                rect: IRect::from_xywh(0, line_h.saturating_mul(4), 100, line_h),
                width: 100,
                height: 100,
            }),
            "the appended item's row (index 4, row 4) is the pending scope"
        );
    }

    /// The row rect is the on-screen band of an item under the current scroll
    /// offset — item 4 sits at screen row 1 with `first_visible` 3, an item
    /// above the viewport has no on-screen pixels.
    #[test]
    fn row_rect_tracks_the_scroll_offset() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 10);
        {
            let ControlState::ListBox { first_visible, .. } = state
                .window_state()
                .control_states
                .get_mut(&Hwnd::from(hwnd))
                .expect("listbox state")
            else {
                panic!("listbox state");
            };
            *first_visible = 3;
        }
        let line_h = listbox_line_height(&mut state, hwnd);
        assert_eq!(
            listbox_row_rect(&mut state, hwnd, 4),
            Some(IRect::from_xywh(0, line_h, 100, line_h)),
            "item 4 is screen row 1 under first_visible 3"
        );
        assert_eq!(
            listbox_row_rect(&mut state, hwnd, 2),
            None,
            "an item above the viewport has no on-screen row"
        );
        assert_eq!(
            listbox_row_rect(&mut state, hwnd, 99),
            None,
            "an item far below the client has no on-screen row"
        );
        assert_eq!(
            listbox_row_rect(&mut state, hwnd, -1),
            None,
            "a cleared selection marks nothing"
        );
    }

    /// The visible band is the FIXED screen viewport — rows `0..visible` at
    /// the client top — regardless of the scroll offset.
    #[test]
    fn band_rect_is_the_screen_viewport_not_the_item_offset() {
        let mut state = test_state();
        let hwnd = tall_listbox_window(&mut state);
        seed_listbox(&mut state, hwnd, 10);
        let line_h = listbox_line_height(&mut state, hwnd);
        let visible = listbox_visible_rows(&mut state, hwnd);
        let band_h = i32::try_from(visible)
            .unwrap_or(0)
            .saturating_mul(line_h)
            .min(100);
        let expected = IRect::from_xywh(0, 0, 100, band_h);
        assert_eq!(listbox_band_rect(&mut state, hwnd), expected);
        {
            let ControlState::ListBox { first_visible, .. } = state
                .window_state()
                .control_states
                .get_mut(&Hwnd::from(hwnd))
                .expect("listbox state")
            else {
                panic!("listbox state");
            };
            *first_visible = 5;
        }
        assert_eq!(
            listbox_band_rect(&mut state, hwnd),
            expected,
            "scrolling changes which items fill the band, not the band itself"
        );
    }
}
