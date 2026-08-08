//! EDIT paint tests: row-level caret repaint, partial-repaint pixel confinement, caret / selection rendering in the ancestor surface, multiline first paint, and the scrollbar chrome / hit-test paths.
use super::*;

/// The caret-blink repaint must narrow to the caret's row: one `WM_TIMER`
/// tick (blink off) repaints only the row holding the caret, and the
/// published frame differs from the previous one ONLY inside that row's
/// y band — the other rows keep their exact pixels.
#[test]
fn test_edit_caret_blink_repaints_only_the_caret_row() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, edit) = push_multiline_edit_pair(&mut state);

    // Focus + a full first paint; capture the frame with the caret drawn on
    // the first row.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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

    // One blink tick hides the caret and must dirty exactly the caret row.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    assert!(
        matches!(
            control_ui(&state, edit).invalid_rows,
            crate::user32::controls::EditInvalidation::Band(band)
                if band.lo == 0 && band.hi == 0
        ),
        "the blink must dirty only the caret row, got {:?}",
        control_ui(&state, edit).invalid_rows
    );

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
    assert_eq!(
        control_ui(&state, edit).last_paint_rows,
        1,
        "the blink repaints only the caret row"
    );
    // The frames differ ONLY inside the caret row's y band: the edit sits at
    // (10, 10), the default font's 16 px row 0 spans surface y 10..26.
    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if before.pixels.get(idx) != after.pixels.get(idx) {
                assert!(
                    (10..26).contains(&y),
                    "a pixel diff at ({x},{y}) lies outside the caret row band"
                );
                diffs = diffs.saturating_add(1);
            }
        }
    }
    assert!(diffs > 0, "hiding the caret must change the painted pixels");
}

/// The row-level invalidation gate: typing one character at the caret paints
/// only the changed row (the coverage counter), while a font change resets
/// the pending band so the next paint still covers every visible row.
#[test]
fn test_edit_row_level_invalidation_typing_narrows_and_font_resets() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_top, edit) = push_multiline_edit_pair(&mut state);

    // Focus + a full first paint: a fresh control paints every visible row
    // (3 rows in a 60 px client at the 16 px default line height).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    assert_eq!(
        control_ui(&state, edit).last_paint_rows,
        3,
        "the first paint covers all three rows"
    );
    assert_eq!(
        control_ui(&state, edit).invalid_rows,
        crate::user32::controls::EditInvalidation::Clean,
        "the paint consumes the pending band"
    );

    // Type one character at the caret (row 0): the pending band is exactly
    // row 0, and the next paint repaints only that row.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect("char ok")
    .expect("some result");
    assert!(
        matches!(
            control_ui(&state, edit).invalid_rows,
            crate::user32::controls::EditInvalidation::Band(band)
                if band.lo == 0 && band.hi == 0
        ),
        "typing at the caret must dirty only row 0, got {:?}",
        control_ui(&state, edit).invalid_rows
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    assert_eq!(
        control_ui(&state, edit).last_paint_rows,
        1,
        "typing one char repaints only the changed row"
    );

    // A font change (with redraw) resets any pending band: the next paint is
    // full again, whatever the new font's line height.
    let font = state
        .gdi_state()
        .alloc_font("Courier New".to_owned(), -16, 700, false, 0);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        1,
    )
    .expect("setfont ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).invalid_rows,
        crate::user32::controls::EditInvalidation::Full,
        "WM_SETFONT resets the pending band to full"
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint3 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    assert!(
        control_ui(&state, edit).last_paint_rows > 1,
        "a font change still full-repaints (got {} rows)",
        control_ui(&state, edit).last_paint_rows
    );
    assert_eq!(
        control_ui(&state, edit).invalid_rows,
        crate::user32::controls::EditInvalidation::Clean,
        "the full repaint consumes the band"
    );
}

/// The pixel gate of the row-level invalidation: typing on row 0 and
/// repainting must leave rows 1–2 BYTE-IDENTICAL — a partial repaint must
/// never wipe the rows outside the dirty band.
#[test]
fn test_edit_partial_repaint_preserves_unpainted_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, edit) = push_multiline_edit_pair(&mut state);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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

    // Type at the caret (row 0) and repaint — only row 0 may change.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect("char ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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

    // The edit sits at (10, 10); the 16 px rows span surface y 26..58 for
    // rows 1 and 2 — every pixel there must be identical to the pre-typing
    // frame (the caret row 0, y 10..26, is allowed to differ).
    let mut row0_diffs = 0_usize;
    for y in 10_i32..58_i32 {
        for x in 10_i32..130_i32 {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            let before_px = before.pixels.get(idx).copied();
            let after_px = after.pixels.get(idx).copied();
            if y < 26 {
                if before_px != after_px {
                    row0_diffs = row0_diffs.saturating_add(1);
                }
            } else {
                assert_eq!(
                    after_px, before_px,
                    "typing on row 0 must not change a pixel on row {y}"
                );
            }
        }
    }
    assert!(row0_diffs > 0, "typing on row 0 must change its own pixels");
}

#[test]
fn test_edit_paint_draws_caret_and_selection_in_ancestor_surface() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, edit) = push_edit_paint_pair(&mut state);

    // Focus + select "el" (chars 1..3) → caret at 3.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        3,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    // B3.5: the paint deferred its publish; flush it (the runtime drains
    // pending publishes once per message dispatch).
    state.present().drain_pending_publishes();

    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();
    assert_eq!((frame.width, frame.height), (200, 100));
    // Font-dependent pixels: assert qualitatively instead of at fixed
    // monospace positions. The selection fill (COLOR_HIGHLIGHT) must be
    // present somewhere in the edit's rows.
    let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
    assert!(
        highlight_count > 0,
        "selected cells must be filled with COLOR_HIGHLIGHT"
    );
    let focused_frame = frame.clone();

    // KILLFOCUS + repaint: no caret, no selection fill.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KILLFOCUS,
        0,
        0,
    )
    .expect("killfocus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
    let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
    assert_eq!(highlight_count, 0, "no highlight without focus");
    // The caret column is font-dependent (its x is the summed advance of
    // the preceding chars, and glyph ink may reach it), so prove the
    // focus change only by the frames differing — the selection fill and
    // caret are the only things that change between them.
    let diffs = frame
        .pixels
        .iter()
        .zip(focused_frame.pixels.iter())
        .filter(|(u, f)| *u != *f)
        .count();
    assert!(diffs > 0, "losing focus must change the painted pixels");
}

#[test]
fn test_edit_paint_empty_text_draws_caret_at_start() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, edit) = push_edit_paint_pair(&mut state);
    // Empty the control's text: the paint must render no glyphs, just the
    // caret bar at the start of the first row.
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .control_text
        .clear();

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
        .expect("published frame")
        .clone();
    // The edit sits at (10, 10), 60x20 in the 200x100 surface; text starts
    // at offset_x + 2 = 12. The caret is the 1 px black bar at column 12;
    // column 13 must stay the COLOR_WINDOW fill — no glyphs.
    let px = |col: i32, row: i32| {
        let idx = (usize::try_from(row).unwrap_or(0) * frame.width as usize)
            .saturating_add(usize::try_from(col).unwrap_or(0));
        frame.pixels[idx]
    };
    let mid = 10_i32.saturating_add(20 / 2); // vertical middle of the edit
    assert_eq!(px(12, mid), 0x0000_0000, "caret bar at the text start");
    assert_eq!(px(13, mid), 0x00FF_FFFF, "no glyphs next to the caret");
}

#[test]
fn test_edit_multiline_first_paint_renders_rows_from_the_top() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Real CreateWindowExW-style records: a plain top-level window and a
    // multiline EDIT child. The creation style (ES_MULTILINE) lands on the
    // window record; the control state is NOT seeded until a message first
    // touches it — so the FIRST WM_PAINT runs against a style-less seed and
    // must still render multiline (exp-61: the stale `style_bits == 0` made
    // the first paint draw a vertically centered block instead of
    // top-aligned rows).
    let top = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
            title: String::new(),
            style: crate::user32::WS_VISIBLE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        },
        true,
    )
    .expect("create top")
    .0;
    let edit = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("EDIT".to_owned()),
            title: String::new(),
            style: crate::user32::WS_CHILD
                | crate::user32::WS_VISIBLE
                | crate::user32::controls::ES_MULTILINE,
            extended_style: 0,
            parent_handle: top,
            menu_handle: 0,
            instance_handle: 0,
            x: 10,
            y: 10,
            width: 120,
            height: 60,
        },
        true,
    )
    .expect("create edit")
    .0;
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.control_text = "first line\nsecond line".to_owned();
        }
    }

    // The FIRST paint — no keyboard/input message ran before it — must draw
    // both lines as TOP-aligned rows (the multiline base_y is the edit's top
    // edge, not the single-line vertical centering).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
        .expect("published frame")
        .clone();

    // Text rows = black glyph ink on the white COLOR_WINDOW fill (the paint
    // erases the edit rect white before rendering). Scan the edit interior —
    // clear of the 1 px black border — for rows that hold ink.
    let frame_width = usize::try_from(frame.width).unwrap_or(0);
    let mut ink_rows: Vec<i32> = Vec::new();
    for y in 12_i32..68_i32 {
        let mut ink = 0_u32;
        for x in 12_i32..128_i32 {
            let idx = (usize::try_from(y).unwrap_or(0))
                .saturating_mul(frame_width)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if frame.pixels.get(idx).copied() == Some(0x0000_0000) {
                ink = ink.saturating_add(1);
            }
        }
        if ink > 0 {
            ink_rows.push(y);
        }
    }
    assert!(
        ink_rows.len() >= 2,
        "two lines must render as at least two distinct rows, got {ink_rows:?}"
    );
    let topmost = ink_rows.first().copied().unwrap_or(0);
    assert!(
        topmost < 20,
        "the first text row must start at the edit's top edge (y 10), got topmost \
         ink row {topmost} (the stale single-line paint vertically centered it)"
    );
}

#[test]
fn test_edit_scrollbar_wrap_width_agrees_with_painted_rows() {
    use crate::user32::controls::{
        edit_text_area, layout_visible_lines, scrollbar_visible, visual_rows,
    };
    // The F5 deferral's core invariant: the shared gutter-aware resolution
    // (edit_text_area — used by both the scroll math and the paint) and the
    // actual painted layout must agree on the row count.
    const ES_MULTILINE: u32 = 0x0004;
    const WS_VSCROLL: u32 = 0x0020_0000;
    const WS_HSCROLL: u32 = 0x0010_0000;
    let advance = &mut |_| 8_i32;

    // 26 chars at 8 px/char: 4 rows at the gutter-reserved wrap width (39 =
    // 60 − 4 − 17), 4 rows at the no-gutter width (56); either way the
    // 3-row viewport overflows, so the V scrollbar reserves its gutter and
    // the paint must lay out at the SAME width.
    let text = "abcdefghijklmnopqrstuvwxyz";
    let style = ES_MULTILINE | WS_VSCROLL;
    let area = edit_text_area(text, 60, 48, 16, style, 0, advance);
    assert!(
        scrollbar_visible(style, area.total, area.visible),
        "26 chars overflow a 3-row viewport"
    );
    assert_eq!(
        area.wrap_width,
        60 - 4 - 17,
        "the V scrollbar reserves the 17 px gutter"
    );
    // The paint path's rows (layout_visible_lines) and the scroll math's total
    // (visual_rows) MUST count the same rows at the shared gutter-reserved
    // width — the deferral's stated reason.
    let painted = layout_visible_lines(text, area.wrap_width, 16, 0, true, 0, advance);
    let (scroll_total, _) = visual_rows(text, area.wrap_width, true, 0, advance);
    assert_eq!(
        painted.len(),
        scroll_total,
        "paint rows == scroll-math rows"
    );
    assert_eq!(
        scroll_total, area.total,
        "scroll math == the shared area total"
    );

    // A 2-line text that fits: no gutter, full wrap width, no V scrollbar.
    let area = edit_text_area("ab\ncd", 60, 48, 16, style, 0, advance);
    assert!(!area.v_scroll_visible, "2 rows fit a 3-row viewport");
    assert_eq!(area.wrap_width, 60 - 4, "no gutter when the content fits");

    // A wrap-off EDIT (WS_HSCROLL): the H scrollbar shows when the widest
    // line overflows, and the visible row count shrinks by its bottom strip.
    let area = edit_text_area(
        "abcdefghijklmnopqrstuvwxyz",
        60,
        48,
        16,
        ES_MULTILINE | WS_HSCROLL,
        0,
        advance,
    );
    assert!(
        area.h_scroll_visible,
        "a 208 px line overflows a 56 px text area"
    );
    assert_eq!(
        area.visible, 1,
        "the 17 px H strip leaves 1 row of a 48 px client"
    );
}

#[test]
fn test_edit_wm_hscroll_moves_first_visible_column() {
    const SB_LINERIGHT: u16 = 1;
    const SB_LINELEFT: u16 = 0;
    const SB_LEFT: u16 = 6;
    const SB_RIGHT: u16 = 7;
    const SB_THUMBTRACK: u16 = 5;
    const WS_HSCROLL: u32 = 0x0010_0000;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "abcdefghijklmnopqrstuvwxyz");
    // Make the edit wrap-OFF: set WS_HSCROLL (the flag notepad's wrap-off
    // edit carries).
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.style |= WS_HSCROLL;
        }
    }

    // hscroll(edit, code, thumb) -> first_visible_column after the scroll.
    let hscroll = |engine: &mut IcedCpu, state: &mut WinApiState, code: u16, thumb: u16| -> usize {
        let wparam = u64::from(code) | (u64::from(thumb) << 16);
        crate::user32::controls::dispatch_control_proc(
            engine,
            state,
            edit,
            crate::user32::wm::WinMsg::WM_HSCROLL.as_u32(),
            wparam,
            0,
        )
        .expect_err("WM_HSCROLL must deliver EN_HSCROLL");
        control_ui(state, edit).first_visible_column
    };

    // A line scroll steps one 8 px character cell (font-independent).
    assert_eq!(hscroll(&mut engine, &mut state, SB_LINERIGHT, 0), 8);
    assert_eq!(hscroll(&mut engine, &mut state, SB_LINERIGHT, 0), 16);
    assert_eq!(hscroll(&mut engine, &mut state, SB_LINELEFT, 0), 8);
    // SB_LEFT jumps to the start; SB_THUMBTRACK sets the raw px offset.
    assert_eq!(hscroll(&mut engine, &mut state, SB_LEFT, 0), 0);
    assert_eq!(hscroll(&mut engine, &mut state, SB_THUMBTRACK, 8), 8);
    // SB_RIGHT jumps to the far end; the offset clamps there (the default
    // font is proportional, so the far end is read back, not hardcoded).
    let far = hscroll(&mut engine, &mut state, SB_RIGHT, 0);
    assert!(
        far >= 16,
        "a 26-char line must overflow the narrow client, got far end {far}"
    );
    assert_eq!(
        hscroll(&mut engine, &mut state, SB_THUMBTRACK, 999),
        far,
        "the offset clamps at the horizontal overflow"
    );
    assert_eq!(
        hscroll(&mut engine, &mut state, SB_LINERIGHT, 0),
        far,
        "a line scroll past the end clamps"
    );

    // Wrap-on EDITs keep the pre-deferral behavior: WM_HSCROLL never moves
    // the offset (there is no horizontal scrollbar to drag).
    let wrap_edit = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("EDIT".to_owned()),
            title: String::new(),
            style: crate::user32::WS_CHILD | crate::user32::controls::ES_MULTILINE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 120,
            height: 20,
        },
        true,
    )
    .expect("create wrap edit")
    .0;
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(wrap_edit) {
            w.control_text = "abcdefghijklmnopqrstuvwxyz".to_owned();
        }
    }
    let wparam = u64::from(SB_RIGHT) | (u64::from(999_u16) << 16);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        wrap_edit,
        crate::user32::wm::WinMsg::WM_HSCROLL.as_u32(),
        wparam,
        0,
    )
    .expect("wrap-on WM_HSCROLL ok")
    .expect("wrap-on WM_HSCROLL result");
    assert_eq!(
        control_ui(&state, wrap_edit).first_visible_column,
        0,
        "wrap-on WM_HSCROLL stays a no-op"
    );
}

#[test]
fn test_edit_scrollbar_chrome_painted_in_gutter() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A real multiline EDIT with WS_VSCROLL: 10 lines in a 5-row client, so
    // the V scrollbar shows and reserves the right 17 px gutter.
    let top = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
            title: String::new(),
            style: crate::user32::WS_VISIBLE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        },
        true,
    )
    .expect("create top")
    .0;
    let edit = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("EDIT".to_owned()),
            title: String::new(),
            style: crate::user32::WS_CHILD
                | crate::user32::WS_VISIBLE
                | crate::user32::controls::ES_MULTILINE
                | 0x0020_0000, // WS_VSCROLL: the chrome shows only when asked
            extended_style: 0,
            parent_handle: top,
            menu_handle: 0,
            instance_handle: 0,
            x: 10,
            y: 10,
            width: 80,
            height: 80,
        },
        true,
    )
    .expect("create edit")
    .0;
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.control_text = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9".to_owned();
        }
    }
    // Size the client to EXACTLY 5 rows of the resolved default font, so the
    // thumb geometry is deterministic (track = height, thumb = track × 5/10).
    let line_h = {
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = font_engine
            .resolve(&crate::gdi32::FontKey::default(), 16)
            .expect("default font")
            .line_height();
        state.gdi_state().font_engine = font_engine;
        line_h
    };
    let track = line_h.saturating_mul(5);
    let thumb = track.saturating_div(2);
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = track;
        }
    }

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
        .expect("published frame")
        .clone();
    let frame_width = usize::try_from(frame.width).unwrap_or(0);
    let px = |col: i32, row: i32| {
        let idx = (usize::try_from(row).unwrap_or(0))
            .saturating_mul(frame_width)
            .saturating_add(usize::try_from(col).unwrap_or(0));
        frame.pixels.get(idx).copied()
    };
    // The edit is 80 px wide at x=10: the gutter spans x [73, 90) (80 − 17).
    // Interior gutter pixel (clear of the 1 px track edges and the border).
    assert_eq!(
        px(80, 10 + thumb + 2),
        Some(0x00F0_F0F0),
        "the gutter interior is BTNFACE"
    );
    assert_eq!(
        px(73, 10 + 5),
        Some(0x00FF_FFFF),
        "the gutter's left edge is BTNHIGHLIGHT"
    );
    assert_eq!(
        px(89, 10 + 5),
        Some(0x00A0_A0A0),
        "the gutter's right edge is BTNSHADOW"
    );
    // Thumb: track = height, 10 rows total, 5 visible → thumb = track/2 at
    // the top (first_visible_line 0); its bottom shadow edge at 10 + thumb − 1.
    assert_eq!(
        px(80, 10 + thumb - 1),
        Some(0x00A0_A0A0),
        "the thumb's bottom edge is BTNSHADOW"
    );
    assert_eq!(
        px(80, 10 + thumb - 4),
        Some(0x00F0_F0F0),
        "the thumb face is BTNFACE"
    );

    // Auto-hide: a 2-line text in the same viewport shows no gutter — the
    // right edge stays the COLOR_WINDOW fill.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.control_text = "ab\ncd".to_owned();
        }
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
        .expect("published frame")
        .clone();
    let frame_width = usize::try_from(frame.width).unwrap_or(0);
    let px = |col: i32, row: i32| {
        let idx = (usize::try_from(row).unwrap_or(0))
            .saturating_mul(frame_width)
            .saturating_add(usize::try_from(col).unwrap_or(0));
        frame.pixels.get(idx).copied()
    };
    assert_eq!(
        px(80, 10 + thumb + 2),
        Some(0x00FF_FFFF),
        "no gutter when the content fits"
    );
}

#[test]
fn test_edit_scrollbar_track_click_pages_and_thumb_drags() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // WS_VSCROLL: the chrome shows only when the style asks for it.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.style |= 0x0020_0000; // WS_VSCROLL
        }
    }
    // Size the client to EXACTLY 5 rows of the resolved default font, so the
    // track/thumb geometry is deterministic: 10 rows total, 5 visible → thumb
    // = track/2 at the top (first_visible_line 0).
    let line_h = {
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = font_engine
            .resolve(&crate::gdi32::FontKey::default(), 16)
            .expect("default font")
            .line_height();
        state.gdi_state().font_engine = font_engine;
        line_h
    };
    let track = line_h.saturating_mul(5);
    let thumb = track.saturating_div(2);
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = track;
        }
    }
    // A click in the track BELOW the thumb pages down by the visible count.
    let gutter_x = u16::try_from(120_i32.saturating_sub(17)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        gutter_x,
        u16::try_from(thumb + 2).unwrap_or(0), // below the thumb
    );
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        5,
        "a track click below the thumb pages down"
    );
    // A click in the track ABOVE the thumb pages up.
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        gutter_x,
        5,
    );
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        0,
        "a track click above the thumb pages up"
    );

    // Thumb drag: grab the thumb (top, at y 5), drag to y 50, release. The
    // travel is track − thumb = track/2 over a 5-row span → a pointer past the
    // travel end lands on the last offset (5).
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        gutter_x,
        5, // inside the thumb (0..thumb)
    );
    assert!(
        control_ui(&state, edit).dragging_scrollbar,
        "a press on the thumb arms the drag"
    );
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        gutter_x,
        u16::try_from(thumb.saturating_add(20)).unwrap_or(0),
    );
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        5,
        "the thumb follows the pointer to the end of the travel"
    );
    release_mouse(
        &mut engine,
        &mut state,
        edit,
        gutter_x,
        u16::try_from(thumb.saturating_add(20)).unwrap_or(0),
    );
    assert!(
        !control_ui(&state, edit).dragging_scrollbar,
        "the release clears the thumb drag"
    );

    // A press in the text area still places the caret (the gutter only
    // consumes presses inside it).
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        20,
        10,
    );
    assert!(
        control_ui(&state, edit).caret > 0,
        "a text-area click still navigates the caret"
    );
}

#[test]
fn test_edit_es_center_first_paint_centers_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A single-line ES_CENTER edit through the REAL creation path: the first
    // paint must read the live alignment (style_bits is stale 0 on the first
    // paint, which would render left-aligned).
    let top = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
            title: String::new(),
            style: crate::user32::WS_VISIBLE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        },
        true,
    )
    .expect("create top")
    .0;
    let edit = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("EDIT".to_owned()),
            title: String::new(),
            style: crate::user32::WS_CHILD | crate::user32::WS_VISIBLE | 0x0001, // ES_CENTER
            extended_style: 0,
            parent_handle: top,
            menu_handle: 0,
            instance_handle: 0,
            x: 10,
            y: 10,
            width: 120,
            height: 20,
        },
        true,
    )
    .expect("create edit")
    .0;
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.control_text = "ab".to_owned();
        }
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
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
        .expect("published frame")
        .clone();
    let frame_width = usize::try_from(frame.width).unwrap_or(0);
    // The ink of the centered "ab" must start well right of the 2 px left
    // margin (a left-aligned paint would put it at x ≈ 12).
    let mut min_ink_x = None;
    let mid = 10_i32.saturating_add(20 / 2);
    for x in 12_i32..130_i32 {
        let idx = (usize::try_from(mid).unwrap_or(0))
            .saturating_mul(frame_width)
            .saturating_add(usize::try_from(x).unwrap_or(0));
        if frame.pixels.get(idx).copied() == Some(0x0000_0000) {
            min_ink_x = Some(x);
            break;
        }
    }
    let min_ink_x = min_ink_x.expect("the centered text must render ink");
    assert!(
        min_ink_x > 40,
        "ES_CENTER must center the first paint, got first ink at x {min_ink_x}"
    );
}
