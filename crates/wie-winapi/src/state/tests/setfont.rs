//! WM_SETFONT / WM_GETFONT tests: font storage on controls and the default window proc, paint-time resolution, measurement effects, and erase-on-redraw.
use super::*;

// ── WM_SETFONT / WM_GETFONT (Task 2.7) ─────────────────────────────────

/// Full WM_SETFONT/WM_GETFONT round trip on an EDIT control: the font comes
/// from a real CreateFontIndirectA dispatch (so the HFONT flows through the
/// GDI font table), WM_SETFONT stores it on the WindowRecord (invalidating
/// when asked), and WM_GETFONT returns 0 until a font is ever set.
#[test]
fn test_wm_setfont_on_control_stores_and_getfont_returns() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_parent, edit) = push_edit_pair(&mut state);

    // WM_GETFONT before any WM_SETFONT: 0 (Windows returns 0 until set).
    let before = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_GETFONT,
        0,
        0,
    )
    .expect("getfont handled")
    .expect("some result");
    assert_eq!(before, 0, "WM_GETFONT must return 0 before any WM_SETFONT");

    // CreateFontIndirectA: LOGFONTA header fields at their Win64 offsets
    // (height 0, weight 16, charset 23), face name char[32] at offset 28.
    let logfont_va = 0x5000_u64;
    engine
        .mem_write(logfont_va, &(-16_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_va + 16, &(700_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_va + 20, &[1_u8])
        .expect("write LOGFONTA.lfItalic");
    engine
        .mem_write(logfont_va + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_va + 28, b"Courier New\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_va, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("gdi32.dll", "CreateFontIndirectA")
        .expect("CreateFontIndirectA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateFontIndirectA must dispatch");
    let font = crate::handles::Hfont::from(r.return_value);
    assert_ne!(
        font,
        crate::handles::Hfont::NULL,
        "CreateFontIndirectA must return an HFONT"
    );

    // WM_SETFONT stores the handle; a non-zero redraw flag invalidates too.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        1,
    )
    .expect("setfont handled")
    .expect("some result");
    assert_eq!(result, 0, "WM_SETFONT must return 0");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit window must exist");
    assert_eq!(
        window.font_handle, font,
        "WM_SETFONT must store the HFONT on the window record"
    );
    assert!(
        window.invalidated,
        "WM_SETFONT with a non-zero redraw flag must invalidate the window"
    );

    // WM_GETFONT returns the stored handle.
    let after = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_GETFONT,
        0,
        0,
    )
    .expect("getfont handled")
    .expect("some result");
    assert_eq!(
        after,
        font.as_u64(),
        "WM_GETFONT must return the stored HFONT"
    );
}

/// DefWindowProc-level WM_SETFONT/WM_GETFONT on a window with a guest
/// WndProc: the guest forwards the message to DefWindowProcA/W, which stores
/// the font the same way the control dispatch does.
#[test]
fn test_def_window_proc_setfont_stores_and_getfont_returns() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, _button) = push_button_pair(&mut state);

    // DefWindowProc(edit, WM_GETFONT) before any set: 0.
    write_regs(
        &mut engine,
        parent,
        u64::from(crate::user32::WM_GETFONT),
        0,
        0,
        0,
    );
    let get = crate::user32::message::handle_def_window_proc_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("DefWindowProcA must dispatch");
    assert_eq!(get.return_value, 0, "WM_GETFONT must return 0 until set");

    // DefWindowProc(edit, WM_SETFONT, hfont, redraw=0).
    let font = state
        .gdi_state()
        .alloc_font("Segoe UI".to_owned(), -16, 400, false, 0);
    write_regs(
        &mut engine,
        parent,
        u64::from(crate::user32::WM_SETFONT),
        font.as_u64(),
        0,
        0,
    );
    let set = crate::user32::message::handle_def_window_proc_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("DefWindowProcA must dispatch");
    assert_eq!(
        set.return_value, 0,
        "DefWindowProc(WM_SETFONT) must return 0"
    );

    // DefWindowProc(edit, WM_GETFONT) now returns the stored handle.
    write_regs(
        &mut engine,
        parent,
        u64::from(crate::user32::WM_GETFONT),
        0,
        0,
        0,
    );
    let get = crate::user32::message::handle_def_window_proc_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("DefWindowProcA must dispatch");
    assert_eq!(
        get.return_value,
        font.as_u64(),
        "DefWindowProc(WM_GETFONT) must return the stored HFONT"
    );
}

/// The paint font resolution: a control with a WM_SETFONT font renders with
/// that font (via the gdi32 helper); an unset window or an unknown HFONT
/// falls back to the system default (sans-serif 16 px).
#[test]
fn test_wm_setfont_resolves_at_paint_and_unknown_font_falls_back() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_parent, edit) = push_edit_pair(&mut state);

    // No WM_SETFONT yet: the paint resolves the system default.
    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, resolved) = crate::gdi32::window_font_resolution(&state, edit, &mut font_engine)
        .expect("the default font must resolve");
    assert_eq!(
        key.family, "",
        "an unset window falls back to the default family"
    );
    assert_eq!(key.weight, 400);
    assert_eq!(resolved.height_px, 16, "the default resolves at 16 px");

    // WM_SETFONT a Courier New bold-italic font; paint must resolve THAT font.
    let font = state
        .gdi_state()
        .alloc_font("Courier New".to_owned(), -16, 700, true, 0);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        0,
    )
    .expect("setfont handled")
    .expect("some result");

    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, resolved) = crate::gdi32::window_font_resolution(&state, edit, &mut font_engine)
        .expect("the stored font must resolve");
    assert_eq!(
        key.family, "courier new",
        "paint must resolve the stored face name"
    );
    assert_eq!(key.weight, 700);
    assert!(key.italic, "the stored italic flag must flow into the key");
    assert_eq!(resolved.height_px, 16, "|lfHeight| becomes the px height");

    // An unknown HFONT falls back to the system default (never a hard error).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        0x9999,
        0,
    )
    .expect("setfont handled")
    .expect("some result");
    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, _resolved) = crate::gdi32::window_font_resolution(&state, edit, &mut font_engine)
        .expect("an unknown HFONT must fall back to the default");
    assert_eq!(
        key.family, "",
        "an unknown HFONT must fall back to the default"
    );
    assert_eq!(key.weight, 400);
}

/// SendMessage(WM_SETFONT) to a window with no WndProc at all (not a
/// control, not a dialog): the SendMessage fallthrough applies DefWindowProc
/// semantics and stores/returns the font like every other window kind.
#[test]
fn test_send_message_setfont_on_wndproc_less_window_stores_font() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A plain window with no guest WndProc and no control kind.
    let hwnd = 0x6610_00F0_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        width: 100,
        height: 50,
        ..Default::default()
    });

    let font = state
        .gdi_state()
        .alloc_font("Tahoma".to_owned(), -13, 400, false, 0);
    write_regs(
        &mut engine,
        hwnd,
        u64::from(crate::user32::WM_SETFONT),
        font.as_u64(),
        1,
        0,
    );
    let set = crate::user32::message::handle_send_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SendMessageA must dispatch");
    assert_eq!(set.return_value, 0, "WM_SETFONT must return 0");
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .is_some_and(|w| w.font_handle == font && w.invalidated),
        "SendMessage(WM_SETFONT, redraw=1) must store the font and invalidate"
    );

    write_regs(
        &mut engine,
        hwnd,
        u64::from(crate::user32::WM_GETFONT),
        0,
        0,
        0,
    );
    let get = crate::user32::message::handle_send_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SendMessageA must dispatch");
    assert_eq!(
        get.return_value,
        font.as_u64(),
        "SendMessage(WM_GETFONT) must return the stored HFONT"
    );
}

/// A 32 px font created through the real CreateFontIndirectA dispatch — the
/// fixture for the stored-font measurement tests (32 px vs the 16 px default
/// makes the line-height math measurably different).
fn push_32px_font(engine: &mut IcedCpu, state: &mut WinApiState) -> crate::handles::Hfont {
    // LOGFONTA header fields at their Win64 offsets (lfHeight at 0, lfWeight
    // at 16, lfCharSet at 23), face name char[32] at offset 28; |lfHeight|
    // becomes the px height.
    let logfont_va = 0x5000_u64;
    engine
        .mem_write(logfont_va, &(-32_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_va + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_va + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_va + 28, b"Segoe UI\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(engine, logfont_va, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("gdi32.dll", "CreateFontIndirectA")
        .expect("CreateFontIndirectA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("CreateFontIndirectA must dispatch");
    let font = crate::handles::Hfont::from(r.return_value);
    assert_ne!(
        font,
        crate::handles::Hfont::NULL,
        "CreateFontIndirectA must return an HFONT"
    );
    font
}

/// A WM_SETFONT'd 32 px font must drive the EDIT measurement paths — the
/// scroll context (visible rows / WM_VSCROLL page), the click-to-caret hit
/// test, EM_POSFROMCHAR's y, and the PgUp/PgDn page size — through the SAME
/// stored-font resolution the paint path uses. Before the fix those four
/// paths hardcoded the 16 px default, so a non-default font made the scroll
/// math, the caret, and EM_POSFROMCHAR disagree with the painted text.
#[test]
fn test_wm_setfont_stored_font_drives_edit_measurements() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    let font = push_32px_font(&mut engine, &mut state);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        0,
    )
    .expect("setfont handled")
    .expect("some result");

    // The stored font's real line height — the metric the measurement paths
    // must now resolve (the same helper the paint path uses).
    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, resolved) = crate::gdi32::window_font_resolution(&state, edit, &mut font_engine)
        .expect("the stored font must resolve");
    assert_eq!(key.family, "segoe ui", "the stored face name must resolve");
    let line_h = resolved.line_height();
    let default_line_h = font_engine
        .resolve(&crate::gdi32::FontKey::default(), 16)
        .expect("default font")
        .line_height();
    assert_ne!(
        line_h, default_line_h,
        "the 32 px fixture must measure differently from the 16 px default"
    );

    // A 3-line-tall client (3 × the STORED line height): 3 rows fit at 32 px
    // where ~6 would fit at the 16 px default.
    let ws = state.window_state();
    if let Some(w) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
    {
        w.height = line_h.saturating_mul(3);
    }

    // EM_POSFROMCHAR: line 1's y is one STORED-font line height, not 16.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        2, // '1', line 1
        0x4000,
    )
    .expect("posfromchar ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_POSFROMCHAR returns TRUE for a valid index");
    let mut bytes = [0_u8; 8];
    engine.mem_read(0x4000, &mut bytes).expect("read point");
    let y = i32::from_le_bytes(bytes[4..8].try_into().expect("y"));
    assert_eq!(
        y, line_h,
        "EM_POSFROMCHAR y must use the stored line height"
    );

    // Click-to-caret: a y in the middle of row 1 (line '1') must land on that
    // row's first char — with the 16 px default the same y lands a row lower.
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        u16::try_from(2).unwrap_or(0),
        u16::try_from(line_h + line_h / 2).unwrap_or(0),
    );
    assert_eq!(
        control_ui(&state, edit).caret,
        2,
        "the click at row 1 must land on '1' (char 2) with the stored font"
    );

    // PgDn page size: client height / stored line height = 3 rows (the 16 px
    // default would page ~6).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        0,
    )
    .expect("setsel ok")
    .expect("some result");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_NEXT);
    assert_eq!(
        control_ui(&state, edit).caret,
        6, // line 3 ('3')
        "PgDn must page by the stored font's line height"
    );
    // The keydown auto-scrolled the caret into view (F5: caret moves scroll),
    // so the SB_PAGEDOWN delta — not the absolute offset — proves the page
    // size: 3 rows fit at 3×line_h (the 16 px default would page ~6).
    const SB_PAGEDOWN: u16 = 3;
    let before_scroll = control_ui(&state, edit).first_visible_line;
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_PAGEDOWN, 0),
        before_scroll + 3,
        "SB_PAGEDOWN must page by the stored font's visible row count"
    );
}

/// F3 regression: RNotepad requests "Lucida Console" with FIXED_PITCH|FF_MODERN
/// (settings.c) and WM_SETFONTs the resulting HFONT onto the EDIT. The full
/// path — CreateFontIndirectA → GDI font table → WM_SETFONT →
/// window_font_resolution → FontKey → face_id_for — must land on a MONOSPACE
/// face. A proportional face (the pre-L8 sans-serif fallback) would break the
/// monospace tell `avg == max`. Host-independent: the tell is a property of
/// any fixed-pitch face, no specific installed family is assumed.
#[test]
fn test_lucida_console_fixed_pitch_resolves_to_monospace_through_edit() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // LOGFONTA: lfHeight=0 lfWeight=16 lfCharSet=23 lfPitchAndFamily=27
    // lfFaceName char[32] at 28. FIXED_PITCH(0x01)|FF_MODERN(0x30) = 0x31.
    let logfont_va = 0x5000_u64;
    engine
        .mem_write(logfont_va, &(-16_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_va + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_va + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_va + 27, &[0x31_u8])
        .expect("write LOGFONTA.lfPitchAndFamily");
    engine
        .mem_write(logfont_va + 28, b"Lucida Console\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_va, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("gdi32.dll", "CreateFontIndirectA")
        .expect("CreateFontIndirectA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateFontIndirectA must dispatch");
    let font = crate::handles::Hfont::from(r.return_value);
    assert_ne!(
        font,
        crate::handles::Hfont::NULL,
        "CreateFontIndirectA must return an HFONT"
    );

    // The pitch hint must land on the font record (FIXED_PITCH bit set).
    let record = state
        .gdi_state()
        .find_font(font)
        .expect("the returned HFONT must resolve to a font record");
    assert_ne!(record.pitch & 0x01, 0, "FIXED_PITCH must be recorded");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        0,
    )
    .expect("setfont handled")
    .expect("some result");

    // The EDIT's stored-font resolution must land on a monospace face: for a
    // proportional face (the pre-L8 fallback) max_advance > avg_advance.
    let mut font_engine = crate::gdi32::FontEngine::default();
    let (key, resolved) = crate::gdi32::window_font_resolution(&state, edit, &mut font_engine)
        .expect("the stored Lucida Console font must resolve");
    assert_eq!(key.family, "lucida console");
    assert!(
        key.fixed_pitch,
        "the FIXED_PITCH bit must reach the FontKey"
    );
    assert_eq!(
        resolved.avg_advance, resolved.max_advance,
        "Lucida Console+FIXED_PITCH must resolve to a monospace face (avg {} max {})",
        resolved.avg_advance, resolved.max_advance
    );
}

/// WM_SETFONT with a non-zero redraw flag must enter the erase/paint cycle
/// exactly like SetWindowPlacement and the resize path: invalidated AND a
/// pending WM_ERASEBKGND (real Windows erases the background before the
/// repaint). redraw=0 stores the font without requesting either.
#[test]
fn test_wm_setfont_redraw_erases_background() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    let font = state
        .gdi_state()
        .alloc_font("Segoe UI".to_owned(), -16, 400, false, 0);

    // redraw=0: store the font, request no repaint at all.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        0,
    )
    .expect("setfont handled")
    .expect("some result");
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit window must exist");
    assert_eq!(window.font_handle, font);
    assert!(
        !window.invalidated,
        "WM_SETFONT with redraw=0 must not invalidate"
    );
    assert!(
        !window.flags.contains(WindowFlags::ERASE_BACKGROUND),
        "WM_SETFONT with redraw=0 must not request an erase"
    );

    // redraw=1: real Windows sends WM_ERASEBKGND before the repaint, so the
    // window must invalidate AND carry a pending erase.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        1,
    )
    .expect("setfont handled")
    .expect("some result");
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit window must exist");
    assert!(
        window.invalidated,
        "WM_SETFONT with redraw=1 must invalidate"
    );
    assert!(
        window.flags.contains(WindowFlags::ERASE_BACKGROUND),
        "WM_SETFONT with redraw=1 must request an erase like real Windows"
    );
}
