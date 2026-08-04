//! LISTBOX/COMBOBOX item logic (selection notifications, hit-testing,
//! viewport scrolling) and the shared text-rendering helper (split from
//! `controls.rs`).

use anyhow::Result;

use super::paint::fill_rect_clipped;
use super::{
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, ControlState, Dimension, PaintCtx, PaintFont,
    control_items, control_sel_index, control_state, control_state_mut, deliver_command,
};
use crate::gdi32::render_text_into_surface;
use crate::gdi32::{FontKey, IRect, ResolvedWindow};
use crate::user32::{LBN_SELCHANGE, VK_DOWN, VK_UP, WinApiState, find_window, make_command_wparam};

/// Draw the LISTBOX items (one line each) from the viewport's first-visible
/// index, filling the selected item's row with COLOR_HIGHLIGHT and rendering
/// its glyphs in COLOR_HIGHLIGHTTEXT (Windows' selected-listbox look). The
/// scroll offset is read from the control state via the DC's own hwnd
/// (`resolve_window_ancestor` sets `dc_window` to the painting control), so
/// the caller does not need to slice the item list.
pub(super) fn paint_item_lines(
    ctx: &mut PaintCtx<'_>,
    info: &ResolvedWindow,
    items: &[String],
    area: Dimension,
    sel_index: i32,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(area.width);
    let bottom = y.saturating_add(area.height);
    let line_h = font.resolved.line_height();
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
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let line_h = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine) {
        Some((_key, resolved)) => resolved.line_height(),
        None => font_engine
            .resolve(&default_key, 16)
            .map_or(16, |resolved| resolved.line_height()),
    };
    state.gdi_state().font_engine = font_engine;
    line_h
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
    // a no-op while a 120-unit notch scrolls a full 3 rows.
    let rows = i64::from(delta.saturating_div(120).saturating_mul(3));
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
    let ControlState::ListBox { first_visible, .. } = control_state_mut(state, hwnd) else {
        return false;
    };
    if *first_visible == clamped {
        return false;
    }
    *first_visible = clamped;
    true
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
    let ControlState::ListBox { first_visible, .. } = control_state_mut(state, hwnd) else {
        return false;
    };
    if *first_visible == clamped {
        return false;
    }
    *first_visible = clamped;
    true
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
    changed
}

#[cfg(test)]
mod tests {
    use super::{
        listbox_hit_item, listbox_key_move_selection, listbox_line_height,
        listbox_scroll_selection_into_view, listbox_scroll_wheel, listbox_visible_rows,
    };
    use crate::guest_heap::GuestHeap;
    use crate::handles::Hwnd;
    use crate::present::MessageQueue;
    use crate::state::{FileIoState, HeapState, ProcessState};
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::controls::{ControlClassKind, ControlState};
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
}
