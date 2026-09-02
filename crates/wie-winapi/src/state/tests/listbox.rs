//! LISTBOX tests: LB_SETCURSEL / LB_GETCURSEL + LBN_SELCHANGE, click selection, selected-row paint highlighting, and the row-level repaint invalidation regions.
use super::*;

fn push_listbox(state: &mut WinApiState) -> (u64, u64) {
    let parent = 0x6610_0013_u64;
    let listbox = 0x6610_0014_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(parent),
        window_proc: 0x7000_0000,
        title: "Parent".to_owned(),
        width: 200,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(listbox),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::ListBox),
        control_text: String::new(),
        menu_handle: 1,
        visible: true,
        width: 100,
        height: 60,
        ..Default::default()
    });
    (parent, listbox)
}

/// Seed a listbox's items through LB_ADDSTRING (ANSI strings in memory).
fn seed_listbox(engine: &mut IcedCpu, state: &mut WinApiState, listbox: u64) {
    for (va, item) in [(0x4000_u64, "alpha"), (0x5000, "beta")] {
        let mut bytes = item.as_bytes().to_vec();
        bytes.push(0);
        engine.mem_write(va, &bytes).expect("write list item");
        crate::user32::controls::dispatch_control_proc(
            engine,
            state,
            listbox,
            crate::user32::LB_ADDSTRING,
            0,
            va,
        )
        .expect("add ok")
        .expect("some result");
    }
}

#[test]
fn test_listbox_setcursel_getcursel_and_lbn_selchange() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, listbox) = push_listbox(&mut state);
    seed_listbox(&mut engine, &mut state, listbox);

    // Fresh listbox: no selection.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_GETCURSEL,
        0,
        0,
    )
    .expect("getcursel ok")
    .expect("some result");
    assert_eq!(r, u64::MAX, "LB_GETCURSEL = LB_ERR without a selection");

    // LB_SETCURSEL(1) selects the second item and delivers LBN_SELCHANGE.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_SETCURSEL,
        1,
        0,
    );
    let error = result.expect_err("LB_SETCURSEL must deliver LBN_SELCHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0001_0001
        ),
        "LB_SETCURSEL must deliver WM_COMMAND(MAKEWPARAM(1, LBN_SELCHANGE)), \
             got {signal:?}"
    );

    // Now LB_GETCURSEL = 1.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_GETCURSEL,
        0,
        0,
    )
    .expect("getcursel ok")
    .expect("some result");
    assert_eq!(r, 1);

    // Re-selecting the same index does not re-notify (returns 0).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_SETCURSEL,
        1,
        0,
    )
    .expect("same setcursel ok")
    .expect("some result");
    assert_eq!(r, 0);

    // Out-of-range index → LB_ERR, selection unchanged.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_SETCURSEL,
        99,
        0,
    )
    .expect("out-of-range ok")
    .expect("some result");
    assert_eq!(r, u64::MAX, "out-of-range LB_SETCURSEL returns LB_ERR");
    assert_eq!(control_ui(&state, listbox).sel_index, 1);

    // LB_SETCURSEL(-1) clears the selection.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_SETCURSEL,
        u64::from(u32::MAX),
        0,
    )
    .expect_err("clearing the selection delivers LBN_SELCHANGE");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_GETCURSEL,
        0,
        0,
    )
    .expect("getcursel ok")
    .expect("some result");
    assert_eq!(r, u64::MAX, "cleared selection reads back as LB_ERR");
}

#[test]
fn test_listbox_click_selects_item_and_notifies() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, listbox) = push_listbox(&mut state);
    seed_listbox(&mut engine, &mut state, listbox);

    // Click at client (2, 18): row 1 = 18 / 16 → selects "beta".
    let lparam = u64::from(2_u16) | (u64::from(18_u16) << 16);
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_LBUTTONDOWN,
        0,
        lparam,
    );
    let error = result.expect_err("click must deliver LBN_SELCHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent && request.word_parameter == 0x0001_0001
        ),
        "listbox click must deliver WM_COMMAND(MAKEWPARAM(1, LBN_SELCHANGE)), \
             got {signal:?}"
    );
    assert_eq!(control_ui(&state, listbox).sel_index, 1);

    // Clicking the same row again does NOT re-notify.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_LBUTTONDOWN,
        0,
        lparam,
    )
    .expect("same click ok")
    .expect("some result");
    assert_eq!(r, 0);

    // A click below the items (y = 60) selects nothing.
    let below = u64::from(2_u16) | (u64::from(60_u16) << 16);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_LBUTTONDOWN,
        0,
        below,
    )
    .expect("below items ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(
        control_ui(&state, listbox).sel_index,
        1,
        "selection unchanged"
    );
}

#[test]
fn test_listbox_paint_highlights_selected_row() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_parent, listbox) = push_listbox(&mut state);
    seed_listbox(&mut engine, &mut state, listbox);
    // Place the listbox as a child of a real top-level window so the
    // ancestor surface exists at a known size.
    let top = 0x6610_0023_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        title: "Top".to_owned(),
        window_proc: 0x7000_0000,
        visible: true,
        width: 200,
        height: 100,
        ..Default::default()
    });
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(listbox))
        .expect("listbox record")
        .parent_handle = crate::handles::Hwnd::from(top);
    // Position the listbox at (10, 10) so the item rows land on known
    // surface coordinates (push_listbox leaves x/y at their defaults).
    let listbox_record = state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(listbox))
        .expect("listbox record");
    listbox_record.x = 10;
    listbox_record.y = 10;

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::LB_SETCURSEL,
        0,
        0,
    )
    .expect_err("LB_SETCURSEL delivers LBN_SELCHANGE to the parent");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();

    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame");
    // Font-dependent rows (line height varies by system font): assert the
    // selection fill exists somewhere in the top rows and the unselected
    // region below is still COLOR_WINDOW.
    let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
    assert!(
        highlight_count > 0,
        "selected row must be filled with COLOR_HIGHLIGHT"
    );
    let window_count = frame.pixels.iter().filter(|&&p| p == 0x00FF_FFFF).count();
    assert!(window_count > 0, "unselected rows stay COLOR_WINDOW");
}

// ── LISTBOX row-level repaint invalidation (the fix-24 mirror) ──────────

use crate::gdi32::IRect;

/// A top-level window with a tall LISTBOX child (100×100 at (10,10) inside a
/// 200×140 top), so the viewport band is a proper sub-rect of the control.
fn push_listbox_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0051_u64;
    let listbox = 0x6610_0052_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        window_proc: 0x7000_0000,
        title: "Top".to_owned(),
        visible: true,
        width: 200,
        height: 140,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(listbox),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 10,
        width: 100,
        height: 100,
        control_kind: Some(crate::user32::controls::ControlClassKind::ListBox),
        control_text: String::new(),
        visible: true,
        ..Default::default()
    });
    (top, listbox)
}

/// Seed `count` items into a listbox through LB_ADDSTRING (ANSI strings at
/// successive guest VAs).
fn seed_listbox_n(engine: &mut IcedCpu, state: &mut WinApiState, listbox: u64, count: usize) {
    for i in 0..count {
        let va = 0x4000_u64.saturating_add(u64::try_from(i).unwrap_or(0).saturating_mul(0x100));
        let item = format!("item {i}");
        let mut bytes = item.as_bytes().to_vec();
        bytes.push(0);
        engine.mem_write(va, &bytes).expect("write list item");
        crate::user32::controls::dispatch_control_proc(
            engine,
            state,
            listbox,
            crate::user32::LB_ADDSTRING,
            0,
            va,
        )
        .expect("add ok")
        .expect("some result");
    }
}

/// The listbox paint's resolved row pitch — the same `window_font_resolution`
/// fallback the paint path uses, so assertions track the painted rows.
fn listbox_line_height_of(state: &mut WinApiState, hwnd: u64) -> i32 {
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let line_h = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine) {
        Some((_key, resolved)) => resolved.line_height(),
        None => font_engine
            .resolve(&default_key, 16)
            .map_or(16, |resolved| resolved.line_height()),
    };
    state.gdi_state().font_engine = font_engine;
    line_h
}

/// The listbox's screen-space viewport band (surface coords) for the
/// `push_listbox_paint_pair` fixture — rows `0..visible` at the resolved line
/// height, clamped to the 100 px client, offset by (10, 10).
fn listbox_viewport_band(state: &mut WinApiState, listbox: u64) -> IRect {
    let line_h = listbox_line_height_of(state, listbox);
    let visible = i32::try_from(
        usize::try_from(100_i32.saturating_div(line_h.max(1)))
            .unwrap_or(0)
            .max(1),
    )
    .unwrap_or(0);
    let band_h = visible.saturating_mul(line_h).min(100);
    IRect {
        left: 10,
        top: 10,
        right: 110,
        bottom: 10_i32.saturating_add(band_h),
    }
}

/// Assert every pixel that differs between `before` and `after` lies inside
/// `region` (surface coords), returning the diff count (the fix-24 pixel-diff
/// confinement style).
fn assert_diffs_confined_in(
    before: &present::SurfaceFrame,
    after: &present::SurfaceFrame,
    region: IRect,
) -> usize {
    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(after.stride).unwrap_or(0))
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if before.pixels.get(idx) != after.pixels.get(idx) {
                assert!(
                    i64::from(x) >= i64::from(region.left)
                        && i64::from(x) < i64::from(region.right)
                        && i64::from(y) >= i64::from(region.top)
                        && i64::from(y) < i64::from(region.bottom),
                    "a pixel diff at ({x},{y}) lies outside the reported region"
                );
                diffs = diffs.saturating_add(1);
            }
        }
    }
    diffs
}

/// The first paint of a LISTBOX covers its whole rect — the surface behind a
/// never-painted control is undefined, so the region cannot be narrowed.
#[test]
fn test_listbox_first_paint_reports_the_full_control_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, listbox) = push_listbox_paint_pair(&mut state);
    seed_listbox_n(&mut engine, &mut state, listbox, 12);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame");
    assert_eq!(
        frame.region, None,
        "the first paint reports the full surface (the fresh accumulator \
         starts fully dirty and a partial mark cannot narrow it)"
    );
}

/// A wheel scroll must report ONLY the viewport band — the scrolled-away rows
/// are erased, the newly-exposed rows painted — not the full control rect,
/// and every pixel change must land inside that band.
#[test]
fn test_listbox_scroll_paints_only_the_viewport_band() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, listbox) = push_listbox_paint_pair(&mut state);
    seed_listbox_n(&mut engine, &mut state, listbox, 12);

    // First paint (full); capture the frame at first_visible 0.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let before = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    // Wheel down one notch: first_visible 0 → 3.
    let wheel_down = u64::from(u16::MAX - 119) << 16; // delta = -120
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::wm::WinMsg::WM_MOUSEWHEEL.as_u32(),
        wheel_down,
        0,
    )
    .expect("wheel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let after = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    let region = after.region.expect("a scroll is a partial repaint");
    let band = listbox_viewport_band(&mut state, listbox);
    assert_eq!(
        region, band,
        "a wheel scroll must report exactly the visible row band"
    );
    let diffs = assert_diffs_confined_in(&before, &after, region);
    assert!(diffs > 0, "the scroll must repaint pixels");
}

/// A selection change (LB_SETCURSEL) must report ONLY the old+new selected
/// rows' union — the highlight swaps between them — not the whole viewport.
#[test]
fn test_listbox_selection_paints_only_the_highlight_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, listbox) = push_listbox_paint_pair(&mut state);
    seed_listbox_n(&mut engine, &mut state, listbox, 12);

    // First paint (full) with no selection.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();

    // Select item 0: the mark is row 0 alone (nothing selected before).
    let line_h = listbox_line_height_of(&mut state, listbox);
    let select = |engine: &mut IcedCpu, state: &mut WinApiState, index: u64| {
        let result = crate::user32::controls::dispatch_control_proc(
            engine,
            state,
            listbox,
            crate::user32::LB_SETCURSEL,
            index,
            0,
        );
        match result {
            // A changed selection delivers LBN_SELCHANGE to the parent.
            Err(error) => assert!(
                error
                    .downcast_ref::<WinApiControlSignal>()
                    .is_some_and(|signal| matches!(
                        signal,
                        WinApiControlSignal::GuestCallbackRequested { .. }
                    )),
                "LB_SETCURSEL must bridge LBN_SELCHANGE, got {error:?}"
            ),
            Ok(value) => assert_eq!(value, Some(0), "an unchanged selection answers 0"),
        }
    };
    select(&mut engine, &mut state, 0);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame");
    assert_eq!(
        frame.region,
        Some(IRect {
            left: 10,
            top: 10,
            right: 110,
            bottom: 10_i32.saturating_add(line_h),
        }),
        "selecting the first row reports only its row band"
    );
    let after_row0 = frame.clone();

    // Move the selection to item 1: rows 0 and 1 swap their highlight, so the
    // pending scope is their union — a two-row band, not the whole viewport.
    select(&mut engine, &mut state, 1);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        listbox,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint3 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let after = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    let region = after
        .region
        .expect("a selection change is a partial repaint");
    let expected = IRect {
        left: 10,
        top: 10,
        right: 110,
        bottom: 10_i32.saturating_add(line_h.saturating_mul(2)),
    };
    assert_eq!(
        region, expected,
        "moving the selection one row reports the two-row union"
    );
    assert!(
        region.height() < 100,
        "the region is the highlight rows, not the whole control, got {region:?}"
    );
    let diffs = assert_diffs_confined_in(&after_row0, &after, region);
    assert!(diffs > 0, "the selection change must repaint pixels");
}

#[test]
fn test_invalidate_rect_partial_publish_region() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Top-level 200×100 window (parentless → its own surface).
    let hwnd = 0x6610_00AA_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        window_proc: 0x7000_0000,
        width: 200,
        height: 100,
        visible: true,
        ..Default::default()
    });

    // First paint: full-window fill on a fresh surface must publish a FULL
    // frame (region None) — a fresh buffer has no up-to-date pixels.
    gdi32::fill_rect_surface(
        &mut state,
        crate::handles::Hwnd::from(hwnd),
        200,
        100,
        0,
        0,
        200,
        100,
        0,
    );
    // B3.5: the fill deferred its publish; flush it (the runtime drains
    // pending publishes once per message dispatch).
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(hwnd))
        .expect("full frame");
    assert_eq!((frame.width, frame.height), (200, 100));
    assert_eq!(frame.pixels.len(), 256 * 100); // stride 200 → 64-padded 256
    // Every publish is full — no region.
    // Partial InvalidateRect(hwnd, {10,20,40,60}): the WM_PAINT synthesis
    // flag is set AND the publish-side dirty rect accumulates the rect.
    write_regs(&mut engine, hwnd, 0x2000, 1, 0, 0);
    for (offset, value) in [(0_u64, 10_i32), (4, 20), (8, 40), (12, 60)] {
        engine
            .mem_write(0x2000 + offset, &value.to_le_bytes())
            .expect("write RECT field");
    }
    user32::handle_invalidate_rect(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("InvalidateRect succeeds");
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .is_some_and(|w| w.invalidated),
        "InvalidateRect must still set the WM_PAINT synthesis flag"
    );

    // The repaint fill covers only the invalidated rect (a real WM_PAINT
    // paints its update region). The publish must carry exactly that rect
    // as the frame region — and the full buffer must remain the complete
    // composite (B1 hand-back keeps accumulation intact across publishes).
    gdi32::fill_rect_surface(
        &mut state,
        crate::handles::Hwnd::from(hwnd),
        200,
        100,
        10,
        20,
        30,
        40,
        0x00FF_0000,
    );
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(hwnd))
        .expect("partial frame");
    // Every publish is full — no region.
    assert_eq!((frame.width, frame.height), (200, 100));
    assert_eq!(frame.pixels.len(), 256 * 100); // stride 200 → 64-padded 256

    // A subsequent full-window repaint also publishes full — every
    // publish is full under the rework, regardless of coverage.
    gdi32::fill_rect_surface(
        &mut state,
        crate::handles::Hwnd::from(hwnd),
        200,
        100,
        0,
        0,
        200,
        100,
        0x0000_00FF,
    );
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(hwnd))
        .expect("full frame 2");
    assert_eq!((frame.width, frame.height), (200, 100));
    assert_eq!(frame.pixels.len(), 256 * 100); // stride 200 → 64-padded 256
}
