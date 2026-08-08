//! Comctl32 status-bar tests: STATUSCLASSNAMEW creation, SB_SETPARTS / SB_SETTEXTW / SB_GETTEXTW, and the part-paint paths.
use super::*;

// --- Comctl32 ---

#[test]
fn test_init_common_controls() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    assert_return_value!(
        comctl32::handle_init_common_controls(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

/// Push a pre-existing parent window record (the child created by
/// `CreateStatusWindowA/W` must link to it through `parent_handle`).
fn push_status_parent(state: &mut WinApiState) -> u64 {
    let parent = 0x6610_0001_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(parent),
        title: "Parent".to_owned(),
        ..Default::default()
    });
    parent
}

#[test]
fn test_create_status_window_w_creates_status_bar_child() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let parent = push_status_parent(&mut state);
    let text_addr = 0x5000;
    write_guest_utf16(&mut engine, text_addr, "Ready");
    // rcx = style, rdx = LPCWSTR text, r8 = hwndParent, r9 = wID.
    write_regs(
        &mut engine,
        u64::from(user32::WS_CHILD | user32::WS_VISIBLE),
        text_addr,
        parent,
        1,
        STACK_TOP,
    );
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comctl32.dll", "CreateStatusWindowW")
        .expect("CreateStatusWindowW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateStatusWindowW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_ne!(
        r.return_value, 0,
        "CreateStatusWindowW must return a nonzero HWND"
    );
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(r.return_value))
        .expect("created status bar must have a window record");
    assert_eq!(
        window.class_name, "msctls_statusbar32",
        "STATUSCLASSNAMEW must be the window class"
    );
    assert_eq!(
        window.control_kind,
        Some(crate::user32::controls::ControlClassKind::StatusBar),
        "status bar must be a recognized built-in control class"
    );
    assert_eq!(window.control_text, "Ready", "text must round trip");
    assert_ne!(
        window.style & user32::WS_CHILD,
        0,
        "CreateStatusWindowW must force WS_CHILD"
    );
    assert_eq!(window.menu_handle, 1, "wID becomes the child-window id");
}

#[test]
fn test_create_status_window_w_parents_to_hwnd_parent() {
    // The parent link must resolve through the window-state machinery: the
    // created status bar's parent_handle identifies the hwndParent record.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let parent = push_status_parent(&mut state);
    let text_addr = 0x5000;
    write_guest_utf16(&mut engine, text_addr, "Ready");
    write_regs(
        &mut engine,
        u64::from(user32::WS_CHILD | user32::WS_VISIBLE),
        text_addr,
        parent,
        2,
        STACK_TOP,
    );
    let id = crate::resolve_winapi_id("comctl32.dll", "CreateStatusWindowW")
        .expect("CreateStatusWindowW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateStatusWindowW must dispatch");
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(r.return_value))
        .expect("created status bar must have a window record");
    let parent_handle = window.parent_handle;
    assert_eq!(
        parent_handle,
        crate::handles::Hwnd::from(parent),
        "status bar must be parented to hwndParent"
    );
    // The parent link resolves back to the parent's window record.
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .any(|w| w.handle == parent_handle),
        "parent_handle must name an existing window record"
    );
}

#[test]
fn test_create_status_window_a_mirrors_with_ansi_text() {
    // ANSI variant: same create path, text read as a byte string.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let parent = push_status_parent(&mut state);
    let text_addr = 0x5000;
    write_guest_ansi(&mut engine, text_addr, "Ready");
    write_regs(
        &mut engine,
        u64::from(user32::WS_CHILD | user32::WS_VISIBLE),
        text_addr,
        parent,
        3,
        STACK_TOP,
    );
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comctl32.dll", "CreateStatusWindowA")
        .expect("CreateStatusWindowA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateStatusWindowA must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_ne!(
        r.return_value, 0,
        "CreateStatusWindowA must return a nonzero HWND"
    );
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(r.return_value))
        .expect("created status bar must have a window record");
    assert_eq!(window.class_name, "msctls_statusbar32");
    assert_eq!(window.control_text, "Ready", "ANSI text must round trip");
    assert_eq!(
        window.parent_handle,
        crate::handles::Hwnd::from(parent),
        "ANSI variant must parent to hwndParent"
    );
    assert_ne!(
        window.style & user32::WS_CHILD,
        0,
        "CreateStatusWindowA must force WS_CHILD"
    );
}

// ── Task 3.1: STATUSCLASSNAMEW status bar + SB_* messages ───────────────

/// A top-level window with a STATUSCLASSNAMEW status-bar child, for the SB_*
/// message and paint tests. The bar sits at the bottom strip (y 76..100) of
/// the 200x100 top window — the classic notepad layout.
fn push_status_bar_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0031_u64;
    let bar = 0x6610_0032_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        title: "Top".to_owned(),
        visible: true,
        width: 200,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(bar),
        parent_handle: crate::handles::Hwnd::from(top),
        control_kind: Some(crate::user32::controls::ControlClassKind::StatusBar),
        control_text: String::new(),
        menu_handle: 0x151,
        visible: true,
        x: 0,
        y: 76,
        width: 200,
        height: 24,
        ..Default::default()
    });
    (top, bar)
}

#[test]
fn test_status_bar_class_name_resolves_to_host_class() {
    use crate::user32::WindowClassIdentifier;
    use crate::user32::controls::ControlClassKind;
    assert_eq!(
        ControlClassKind::from_identifier(&WindowClassIdentifier::Name(
            "msctls_statusbar32".to_owned()
        )),
        Some(ControlClassKind::StatusBar),
        "STATUSCLASSNAMEW must resolve to the built-in StatusBar class"
    );
    // Class-name lookup is case-insensitive like the other built-ins.
    assert_eq!(
        ControlClassKind::from_identifier(&WindowClassIdentifier::Name(
            "MSCTLS_STATUSBAR32".to_owned()
        )),
        Some(ControlClassKind::StatusBar)
    );
}

/// Write `parts` as a guest int array at `addr` (the SB_SETPARTS lParam).
fn write_guest_int_array(engine: &mut IcedCpu, addr: u64, parts: &[i32], addr_name: &str) {
    for (index, part) in parts.iter().enumerate() {
        let off = u64::try_from(index.saturating_mul(4)).unwrap_or(0);
        engine
            .mem_write(addr.saturating_add(off), &part.to_le_bytes())
            .expect(addr_name);
    }
}

/// Read a guest int array of `count` entries at `addr`.
fn read_guest_int_array(engine: &mut IcedCpu, addr: u64, count: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let off = u64::try_from(index.saturating_mul(4)).unwrap_or(0);
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(addr.saturating_add(off), &mut bytes)
            .expect("read guest int array");
        out.push(i32::from_le_bytes(bytes));
    }
    out
}

#[test]
fn test_status_bar_sb_setparts_stores_widths_and_getparts_returns_count() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_top, bar) = push_status_bar_pair(&mut state);

    // SB_SETPARTS(3, [80, 160, -1]): -1 = extend to the right edge.
    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[80, 160, -1], "write parts");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts handled")
    .expect("some result");
    assert_eq!(r, 1, "SB_SETPARTS must return TRUE");

    let ui = control_ui(&state, bar);
    assert_eq!(
        ui.part_rights,
        vec![80, 160, -1],
        "SB_SETPARTS must store the part right-edge widths"
    );

    // SB_GETPARTS(4, buffer): copies the stored widths and returns the count.
    let get_addr = 0x6100;
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_GETPARTS,
        4,
        get_addr,
    )
    .expect("getparts handled")
    .expect("some result");
    assert_eq!(r, 3, "SB_GETPARTS must return the part count");
    assert_eq!(
        read_guest_int_array(&mut engine, get_addr, 3),
        vec![80, 160, -1],
        "SB_GETPARTS must copy the stored widths"
    );
}

#[test]
fn test_status_bar_sb_settextw_stores_part_text_and_ignores_flags() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_top, bar) = push_status_bar_pair(&mut state);

    let text_addr = 0x6000;
    write_guest_utf16(&mut engine, text_addr, "Ln 1, Col 1");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        0,
        text_addr,
    )
    .expect("settext ok")
    .expect("some result");
    assert_eq!(r, 1, "SB_SETTEXTW must return TRUE");

    write_guest_utf16(&mut engine, text_addr, "CRLF");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        1,
        text_addr,
    )
    .expect("settext1 ok")
    .expect("some result");

    // SBT_NOBORDERS (0x100) OR'd into the part index is ignored — the part
    // index is the low byte, so 0x100 | 2 still targets part 2.
    write_guest_utf16(&mut engine, text_addr, "UTF-8");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        u64::from(crate::user32::controls::SBT_NOBORDERS | 2),
        text_addr,
    )
    .expect("settext2 ok")
    .expect("some result");

    let ui = control_ui(&state, bar);
    assert_eq!(
        ui.part_texts,
        vec!["Ln 1, Col 1", "CRLF", "UTF-8"],
        "SB_SETTEXTW must store one text per part"
    );
}

#[test]
fn test_status_bar_sb_gettextw_and_gettextlengthw_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_top, bar) = push_status_bar_pair(&mut state);

    let text_addr = 0x6000;
    write_guest_utf16(&mut engine, text_addr, "Ln 1, Col 1");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        0,
        text_addr,
    )
    .expect("settext ok")
    .expect("some result");

    // SB_GETTEXTLENGTHW(0): the length in WCHARs (excluding the NUL).
    let len = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_GETTEXTLENGTHW,
        0,
        0,
    )
    .expect("getlen ok")
    .expect("some result");
    assert_eq!(len, 11, "SB_GETTEXTLENGTHW must return the char count");

    // SB_GETTEXTW(0, buffer): copies the text, returns the char count.
    let get_addr = 0x6100;
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_GETTEXTW,
        0,
        get_addr,
    )
    .expect("gettext ok")
    .expect("some result");
    assert_eq!(r, 11, "SB_GETTEXTW must return the char count");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, get_addr, 64),
        "Ln 1, Col 1",
        "SB_GETTEXTW must round-trip the stored text"
    );
}

#[test]
fn test_status_bar_paint_draws_parts_with_client_edge_and_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);

    // 3 parts [80, 160, -1]; part 0 stays empty, parts 1/2 get text.
    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[80, 160, -1], "write parts");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts ok")
    .expect("some result");
    let text_addr = 0x6100;
    write_guest_utf16(&mut engine, text_addr, "EOLN");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        1,
        text_addr,
    )
    .expect("settext1 ok")
    .expect("some result");
    write_guest_utf16(&mut engine, text_addr, "UTF-8");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        2,
        text_addr,
    )
    .expect("settext2 ok")
    .expect("some result");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    // The paint deferred its publish; flush it like the runtime does.
    state.present().drain_pending_publishes();

    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();
    assert_eq!((frame.width, frame.height), (200, 100));
    let px = |col: i32, row: i32| -> u32 {
        let idx = (usize::try_from(row).unwrap_or(0) * usize::try_from(frame.width).unwrap_or(0))
            .saturating_add(usize::try_from(col).unwrap_or(0));
        frame.pixels.get(idx).copied().unwrap_or(0)
    };

    // The strip sits at the bottom (y 76..100): BTNFACE fill with a light
    // client edge on top and the shadow edge at the bottom.
    assert_eq!(px(10, 77), 0x00F0_F0F0, "BTNFACE inside the strip");
    assert_eq!(px(10, 76), 0x00FF_FFFF, "light client edge on the top");
    assert_eq!(px(10, 99), 0x00A0_A0A0, "shadow edge at the bottom");

    // The text is inset below the border: no ink on the edge rows.
    assert_eq!(px(180, 76), 0x00FF_FFFF, "no text ink on the top edge");

    // Non-face ink in the strip interior (rows 78..98), per part cell.
    let ink_in = |range: std::ops::Range<i32>| -> usize {
        range
            .filter(|col| (78..98).any(|row| px(*col, row) != 0x00F0_F0F0))
            .count()
    };
    assert_eq!(ink_in(10..80), 0, "an empty part stays the plain strip");
    assert!(ink_in(81..160) > 0, "part 1 text (EOLN) must render ink");
    assert!(ink_in(161..200) > 0, "part 2 text (UTF-8) must render ink");
}

#[test]
fn test_status_bar_paint_draws_part_separator_grooves() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);

    // 3 parts [80, 160, -1]; -1 = the last part extends to the right edge.
    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[80, 160, -1], "write parts");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts ok")
    .expect("some result");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
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
    let px = |col: i32, row: i32| -> u32 {
        let idx = (usize::try_from(row).unwrap_or(0) * usize::try_from(frame.width).unwrap_or(0))
            .saturating_add(usize::try_from(col).unwrap_or(0));
        frame.pixels.get(idx).copied().unwrap_or(0)
    };

    // Each part boundary (except after the last part) carries the sunken
    // groove: a BTNSHADOW line at the boundary with BTNHIGHLIGHT adjacent.
    assert_eq!(px(80, 90), 0x00A0_A0A0, "groove shadow at boundary 80");
    assert_eq!(px(81, 90), 0x00FF_FFFF, "groove highlight right of 80");
    assert_eq!(px(160, 90), 0x00A0_A0A0, "groove shadow at boundary 160");
    assert_eq!(px(161, 90), 0x00FF_FFFF, "groove highlight right of 160");

    // The last part runs to the strip's right edge: no right separator.
    assert_eq!(px(199, 90), 0x00F0_F0F0, "no separator after the last part");

    // The groove spans only the interior rows (77..98), so its corners join
    // the top highlight and bottom shadow lines cleanly.
    assert_eq!(
        px(80, 76),
        0x00FF_FFFF,
        "top edge continues over the groove"
    );
    assert_eq!(px(80, 77), 0x00A0_A0A0, "groove starts below the top edge");
    assert_eq!(px(80, 98), 0x00A0_A0A0, "groove ends above the bottom edge");
    assert_eq!(
        px(80, 99),
        0x00A0_A0A0,
        "bottom edge continues over the groove"
    );

    // Between-part columns away from the grooves stay the plain face.
    assert_eq!(px(120, 90), 0x00F0_F0F0, "part 1 interior stays BTNFACE");
}

/// Regression: View > Status Bar — a HIDDEN control must not paint.
///
/// `ShowWindow(SW_HIDE)` leaves the bar invalidated (RNotepad's toggle re-sizes
/// the bar, which invalidates it), and the paint synthesizer queues a WM_PAINT
/// for any invalidated window regardless of visibility. Real Windows discards a
/// hidden window's invalid region; the paint-side gate (the `WM_PAINT` arm of
/// `dispatch_control_proc`) skips `paint_control` for invisible windows but
/// still consumes the invalidation so the hidden window cannot re-enter the
/// paint cycle every idle drain.
#[test]
fn test_hidden_status_bar_paint_is_skipped_and_consumes_invalidation() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);

    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[80, 160, -1], "write parts");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts ok")
    .expect("some result");

    // The WM_SIZE aftermath of the toggle: hidden, but still invalidated.
    {
        let windows = &mut state.window_state().windows;
        let bar_record = windows
            .iter_mut()
            .find(|window| window.handle == crate::handles::Hwnd::from(bar))
            .expect("status bar record");
        bar_record.visible = false;
        bar_record.invalidated = true;
    }

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint dispatch ok")
    .expect("paint handled");

    // paint_control never ran: the WM_PAINT arm defers a publish only after a
    // real paint, so a skipped hidden paint emits nothing to the ancestor.
    assert_eq!(
        state.present().drain_pending_publishes(),
        0,
        "a hidden control must not publish a paint into the ancestor surface"
    );
    assert!(
        !state
            .present()
            .published
            .contains_key(&crate::handles::Hwnd::from(top)),
        "no frame may exist for a surface that never painted"
    );

    // The invalidation is consumed while hidden: ShowWindow(SW_SHOW) re-arms
    // it, so the hidden window cannot livelock the paint pump.
    let bar_record = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(bar))
        .expect("status bar record");
    assert!(!bar_record.invalidated, "the invalidation is consumed");
    assert!(!bar_record.visible, "the bar stays hidden");
}

/// Round trip: hide → paint (no new bar content) → show → paint (bar appears).
///
/// Mirrors the View > Status Bar toggle: after the bar is hidden, the parent's
/// repaint covers its old strip region (the strip content is unchanged — the
/// bar never repaints over it); when the bar is shown again the show path
/// re-invalidates it and the bar paints normally.
#[test]
fn test_status_bar_hide_paint_show_paint_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);

    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[80, 160, -1], "write parts");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts ok")
    .expect("some result");
    let text_addr = 0x6100;
    write_guest_utf16(&mut engine, text_addr, "EOLN");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        1,
        text_addr,
    )
    .expect("settext ok")
    .expect("some result");

    // Show + paint: the strip renders at the bottom of the 200x100 top window.
    let frame_before = {
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            bar,
            crate::user32::WM_PAINT,
            0,
            0,
        )
        .expect("paint dispatch ok")
        .expect("paint handled");
        assert_eq!(
            state.present().drain_pending_publishes(),
            1,
            "a visible bar paint defers exactly one ancestor publish"
        );
        state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(top))
            .expect("published frame")
            .clone()
    };
    assert_eq!(
        frame_before.pixels[(usize::try_from(77).unwrap_or(0)
            * usize::try_from(frame_before.width).unwrap_or(0))
            + usize::try_from(10).unwrap_or(0)],
        0x00F0_F0F0,
        "the visible bar paints its BTNFACE strip"
    );

    // Hide (SW_HIDE) while invalidated: the paint is skipped — no new
    // publish, the strip region is unchanged, the invalidation is consumed.
    {
        let windows = &mut state.window_state().windows;
        let bar_record = windows
            .iter_mut()
            .find(|window| window.handle == crate::handles::Hwnd::from(bar))
            .expect("status bar record");
        bar_record.visible = false;
        bar_record.invalidated = true;
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint dispatch ok")
    .expect("paint handled");
    assert_eq!(
        state.present().drain_pending_publishes(),
        0,
        "a hidden bar paint must not publish"
    );
    let frame_after_hide = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();
    assert_eq!(
        frame_after_hide.pixels, frame_before.pixels,
        "the bar's surface region is unchanged by a hidden paint"
    );

    // Show again (SW_SHOW re-arms invalidated): the bar paints normally.
    {
        let windows = &mut state.window_state().windows;
        let bar_record = windows
            .iter_mut()
            .find(|window| window.handle == crate::handles::Hwnd::from(bar))
            .expect("status bar record");
        bar_record.visible = true;
        bar_record.invalidated = true;
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint dispatch ok")
    .expect("paint handled");
    assert_eq!(
        state.present().drain_pending_publishes(),
        1,
        "a shown bar paint publishes the strip again"
    );
    let frame_after_show = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();
    assert_eq!(
        frame_after_show.pixels[(usize::try_from(77).unwrap_or(0)
            * usize::try_from(frame_after_show.width).unwrap_or(0))
            + usize::try_from(10).unwrap_or(0)],
        0x00F0_F0F0,
        "the re-shown bar repaints its BTNFACE strip"
    );
}

/// Red-green regression for the real RNotepad status-bar geometry.
///
/// `DIALOG_StatusBarAlignParts` does NOT measure the part text: it computes
/// the parts from the bar's client width alone (`parts = [W-240, W-120, -1]`,
/// clamped), so the EOL cell ("Windows (CR + LF)") is a FIXED 120 px box
/// sized for the ~13 px default GUI font real Windows draws a font-less
/// status bar with. WIE's 16 px system default renders the text ~125 px wide
/// — wider than the 120 px cell — and the last glyph clips at the boundary.
/// The paint must draw a font-less status bar at the guest-UI font size so
/// the fixed geometry fits, with the right inset keeping the ink off the edge.
#[test]
fn test_status_bar_no_font_fixed_cell_fits_full_part_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);
    // Widen to a real notepad default; the bar keeps the fixture's 24 px
    // height and bottom-strip placement.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(top)
            || w.handle == crate::handles::Hwnd::from(bar)
        {
            w.width = 640;
        }
    }
    // The guest's DIALOG_StatusBarAlignParts output for W = 640: part 1's
    // right edge is W - 120 = 520, so its cell is the fixed [400, 520] box.
    let parts_addr = 0x6000;
    write_guest_int_array(&mut engine, parts_addr, &[400, 520, -1], "write parts");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETPARTS,
        3,
        parts_addr,
    )
    .expect("setparts ok")
    .expect("some result");
    let text_addr = 0x6100;
    write_guest_utf16(&mut engine, text_addr, "Windows (CR + LF)");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        1,
        text_addr,
    )
    .expect("settext1 ok")
    .expect("some result");
    write_guest_utf16(&mut engine, text_addr, "UTF-8");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::controls::SB_SETTEXTW,
        2,
        text_addr,
    )
    .expect("settext2 ok")
    .expect("some result");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
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
    assert_eq!((frame.width, frame.height), (640, 100));
    let ink_in = |range: std::ops::Range<i32>| -> usize {
        let mut count = 0_usize;
        for col in range {
            for row in 78..98 {
                let idx = (usize::try_from(row).unwrap_or(0)
                    * usize::try_from(frame.width).unwrap_or(0))
                .saturating_add(usize::try_from(col).unwrap_or(0));
                if frame.pixels.get(idx).copied().unwrap_or(0) != 0x00F0_F0F0 {
                    count = count.saturating_add(1);
                }
            }
        }
        count
    };
    // The EOL text renders inside the fixed cell (its first half is inked)…
    assert!(
        ink_in(403..480) > 0,
        "Windows (CR + LF) must render inside its fixed 120 px cell"
    );
    // …and the last glyph is FULLY visible: nothing in the final 8 px before
    // the cell boundary (the 16 px default font reaches past it; the ~13 px
    // part font plus the 3 px right inset keeps it clear).
    assert_eq!(
        ink_in(512..520),
        0,
        "the EOL text must not clip at the cell edge"
    );
    // Part 2 still renders after the boundary.
    assert!(ink_in(523..637) > 0, "UTF-8 must render in the last part");
}

/// Verdict pin: `GetTextExtentPoint32` and the status-bar paint resolve the
/// SAME font at the SAME size when the guest selects the bar's font into the
/// measurement DC (the real-app flow: `GetDC` → `SelectObject(WM_GETFONT)` →
/// `GetTextExtentPoint32`). Both paths sum `char_advance` over the same
/// resolved font, so a cell sized to the measured width holds the rendered
/// text — the recon's suspected measurement/render font divergence does not
/// exist on this path (both agree at the stored `WM_SETFONT` font).
#[test]
fn test_status_bar_measurement_matches_paint_font_when_selected() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_top, bar) = push_status_bar_pair(&mut state);

    // A 13 px "MS Shell Dlg" font through the real dispatch path (the guest's
    // own UI font; the HFONT lands in the GDI font table).
    let logfont_va = 0x5000_u64;
    engine
        .mem_write(logfont_va, &(-13_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_va + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_va + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_va + 28, b"MS Shell Dlg\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_va, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("gdi32.dll", "CreateFontIndirectA")
        .expect("CreateFontIndirectA must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateFontIndirectA must dispatch");
    let font = r.return_value;
    assert_ne!(font, 0, "CreateFontIndirectA must return an HFONT");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_SETFONT,
        font,
        0,
    )
    .expect("setfont handled")
    .expect("some result");

    // The guest's measurement DC: GetDC(statusbar), then select the bar's
    // font into it the way a real app does.
    write_regs(&mut engine, bar, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetDC").expect("GetDC must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetDC must dispatch");
    let dc = r.return_value;
    assert_ne!(dc, 0, "GetDC must return an HDC");
    write_regs(&mut engine, dc, font, 0, 0, 0);
    let id =
        crate::resolve_winapi_id("gdi32.dll", "SelectObject").expect("SelectObject must resolve");
    crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("SelectObject must dispatch");

    // Measure the widest part text the way the guest does.
    let text = "Windows (CR + LF)";
    let text_addr = 0x6000_u64;
    write_guest_utf16(&mut engine, text_addr, text);
    let size_addr = 0x6200_u64;
    let count = u64::try_from(text.encode_utf16().count()).unwrap_or(0);
    write_regs(&mut engine, dc, text_addr, count, size_addr, 0);
    let id = crate::resolve_winapi_id("gdi32.dll", "GetTextExtentPoint32W")
        .expect("GetTextExtentPoint32W must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetTextExtentPoint32W must dispatch");
    assert_ne!(r.return_value, 0, "extent must succeed");
    let mut bytes = [0_u8; 8];
    engine.mem_read(size_addr, &mut bytes).expect("read size");
    let measured_w = i32::from_le_bytes(bytes[0..4].try_into().expect("cx"));

    // Paint-side width: the advance sum at the font the paint resolves.
    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, resolved) = crate::gdi32::window_font_resolution(&state, bar, &mut font_engine)
        .expect("paint font must resolve");
    let paint_w = font_engine.text_advance(&resolved, &key, text, text.len());
    assert_eq!(
        measured_w, paint_w,
        "GetTextExtentPoint32 must agree with the paint's advance sum at the \
         stored font"
    );
}

#[test]
fn test_status_bar_wm_size_sizes_and_positions_bar_in_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let top = 0x6610_0041_u64;
    let bar = 0x6610_0042_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        title: "Top".to_owned(),
        visible: true,
        width: 200,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(bar),
        parent_handle: crate::handles::Hwnd::from(top),
        control_kind: Some(crate::user32::controls::ControlClassKind::StatusBar),
        style: crate::user32::WS_CHILD | crate::user32::controls::CCS_BOTTOM,
        visible: true,
        ..Default::default()
    });

    // The default height is the control font's line height plus the two
    // client-edge border rows — the same formula the WM_SIZE handler uses.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16).expect("default font");
    let line_h = resolved.line_height();
    state.gdi_state().font_engine = font_engine;
    let expected_height = line_h.saturating_add(4);

    // notepad sends SendMessageW(hStatusBar, WM_SIZE, 0, 0) after creating
    // the bar; the bar must size itself to the parent client and sit flush
    // at the parent's bottom edge (CCS_BOTTOM).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        bar,
        crate::user32::WM_SIZE,
        0,
        0,
    )
    .expect("wmsize handled")
    .expect("some result");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(bar))
        .expect("bar record");
    assert_eq!(window.width, 200, "the bar spans the parent width");
    assert_eq!(
        window.height, expected_height,
        "the default height is font-derived"
    );
    assert_eq!(window.x, 0);
    assert_eq!(
        window.y,
        100 - expected_height,
        "CCS_BOTTOM: the bar sits flush at the parent bottom"
    );
}

/// Regression: View > Status Bar — hiding a visible CHILD must erase its
/// vacated rect in the owner surface.
///
/// `ShowWindow(SW_HIDE)` used to only flip `visible`: nothing invalidated the
/// owner, so the strip pixels stayed in the surface until a later input's
/// repaint covered them (the "status bar persists until a click" bug — the
/// hidden bar's own paint is gated, so NOTHING ever erased the region). Real
/// Windows repaints the parent's vacated region; WIE now invalidates the
/// owner with erase + a content-revision bump on SW_HIDE.
#[test]
fn test_hide_child_invalidates_owner_with_erase() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, bar) = push_status_bar_pair(&mut state);

    // ShowWindow(bar, SW_HIDE): RCX = hwnd, RDX = 0.
    write_regs(&mut engine, bar, 0, 0, 0, 0);
    dispatch_user32(&mut engine, &mut state, "ShowWindow");

    let ws = state.window_state();
    let bar_record = ws
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(bar))
        .expect("status bar record");
    assert!(!bar_record.visible, "the bar is hidden");

    let top_record = ws
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(top))
        .expect("owner record");
    assert!(
        top_record.invalidated,
        "hiding a child invalidates the owner so the erase covers the vacated rect"
    );
    assert!(
        top_record
            .flags
            .contains(crate::state::WindowFlags::ERASE_BACKGROUND),
        "the owner erase is requested"
    );
}
