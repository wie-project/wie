//! The live RNotepad EDIT repro: the real CreateWindowExW ABI with the notepad EDIT_STYLE_WRAP styles and the full typing/scroll/caret sequence.
use super::*;

// ── Live RNotepad EDIT repro (the real CreateWindowExW ABI) ───────────────

/// The live RNotepad EDIT style (notepad.h `EDIT_STYLE_WRAP`): WS_CHILD |
/// WS_VSCROLL | ES_AUTOVSCROLL | ES_MULTILINE | ES_NOHIDESEL. The ES_* bits
/// are the REAL Windows values from the mingw winuser.h the guest compiles
/// against — in particular ES_MULTILINE is 0x0004, NOT WIE's internal 0x1000
/// (which is really ES_WANTRETURN).
const RN_EDIT_STYLE_WRAP: u32 = 0x4020_0144;
/// The real Windows `ES_MULTILINE` (winuser.h): 0x0004.
const ES_MULTILINE_REAL: u32 = 0x0004;
/// `CW_USEDEFAULT` as the i32 the Win64 stack slot carries it (0x8000_0000).
const CW_USEDEFAULT_I32: i32 = i32::MIN;

/// Drive `CreateWindowExW` through the REAL handler entry point: the four
/// register args (RCX = exStyle, RDX = class, R8 = title, R9 = dwStyle) plus
/// the Win64 stack args at [rsp+0x28..0x60]. The class identifier is a
/// UTF-16 string pointer (the live notepad passes `EDIT_CLASS` by name).
/// Returns the HWND the handler returns — a class without a guest WndProc
/// (an unregistered plain class, or a built-in control) completes
/// synchronously.
fn create_window_ex_w_full(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    ex_style: u32,
    class_va: u64,
    title_va: u64,
    style: u32,
    stack: CwxStackArgs,
) -> u64 {
    write_regs(
        engine,
        u64::from(ex_style),
        class_va,
        title_va,
        u64::from(style),
        STACK_TOP,
    );
    let CwxStackArgs {
        x,
        y,
        width,
        height,
        parent,
        menu,
        instance,
    } = stack;
    for (offset, value) in [
        (0x28_u64, x as u64 & 0xFFFF_FFFF),
        (0x30, y as u64 & 0xFFFF_FFFF),
        (0x38, width as u64 & 0xFFFF_FFFF),
        (0x40, height as u64 & 0xFFFF_FFFF),
        (0x48, parent),
        (0x50, menu),
        (0x58, instance),
        (0x60, 0), // lpParam
    ] {
        engine
            .mem_write(STACK_TOP.saturating_add(offset), &value.to_le_bytes())
            .expect("CreateWindowExW stack arg");
    }
    let id = crate::resolve_winapi_id("user32.dll", "CreateWindowExW")
        .expect("CreateWindowExW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(&mut HandlerContext::new(engine, default_env(), state), id)
        .expect("CreateWindowExW must dispatch");
    r.return_value
}

/// The Win64 stack arguments of `CreateWindowExW`, at [rsp+0x28..0x60].
struct CwxStackArgs {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: u64,
    menu: u64,
    instance: u64,
}

/// The guest's exact status-bar caret computation (dialog.c:933-948):
/// `EM_GETSEL → EM_LINEFROMCHAR → EM_LINEINDEX → col = dwStart - ich` with
/// the `ich < 0 → 0` guard. `dwStart`/`dwEnd` are the DWORDs the guest passes
/// as pointer args, at guest VAs 0x8000/0x8004 here.
fn guest_caret_col(engine: &mut IcedCpu, state: &mut WinApiState, edit: u64) -> u64 {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::EM_GETSEL,
        0x8000,
        0x8004,
    )
    .expect("getsel ok")
    .expect("some result");
    let mut dw_start_bytes = [0_u8; 4];
    engine
        .mem_read(0x8000, &mut dw_start_bytes)
        .expect("read dwStart");
    let dw_start = u32::from_le_bytes(dw_start_bytes);
    let line = crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::EM_LINEFROMCHAR,
        u64::from(dw_start),
        0,
    )
    .expect("linefromchar ok")
    .expect("some result");
    let ich_raw = crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::EM_LINEINDEX,
        line,
        0,
    )
    .expect("lineindex ok")
    .expect("some result");
    let ich = i64::from(i32::from_le_bytes(
        u32::try_from(ich_raw & 0xFFFF_FFFF)
            .unwrap_or(0)
            .to_le_bytes(),
    ));
    if ich < 0 {
        0
    } else {
        u64::from(dw_start).saturating_sub(u64::try_from(ich).unwrap_or(0))
    }
}

#[test]
fn test_rnotepad_edit_live_sequence() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let image_base = default_env().image_base;

    // Parent: a plain top-level window through the real handler. The class
    // name pointer must NOT fit u16 — read_window_class_identifier_w treats
    // any low value as a class ATOM, not a string pointer.
    write_guest_utf16(&mut engine, 0x1_0000, "GuiClass");
    write_guest_utf16(&mut engine, 0x6000, "Notepad");
    let parent = create_window_ex_w_full(
        &mut engine,
        &mut state,
        0,
        0x1_0000,
        0x6000,
        0x00CF_0000, // WS_OVERLAPPEDWINDOW
        CwxStackArgs {
            x: 100,
            y: 100,
            width: 640,
            height: 480,
            parent: 0,
            menu: 0,
            instance: image_base,
        },
    );
    assert_ne!(parent, 0, "parent window must create");

    // The EDIT child: the EXACT live call (dialog.c:709-716) — WS_EX_CLIENTEDGE,
    // class "EDIT", dwStyle = EDIT_STYLE_WRAP, CW_USEDEFAULT for x/y/w/h.
    write_guest_utf16(&mut engine, 0x1_1000, "EDIT");
    let edit = create_window_ex_w_full(
        &mut engine,
        &mut state,
        0x0200, // WS_EX_CLIENTEDGE
        0x1_1000,
        0, // NULL title
        RN_EDIT_STYLE_WRAP,
        CwxStackArgs {
            x: CW_USEDEFAULT_I32,
            y: CW_USEDEFAULT_I32,
            width: CW_USEDEFAULT_I32,
            height: CW_USEDEFAULT_I32,
            parent,
            menu: 0,
            instance: image_base,
        },
    );
    assert_ne!(edit, 0, "EDIT child must create");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record exists");
    assert_eq!(
        window.style, RN_EDIT_STYLE_WRAP,
        "the creation style must arrive unchanged"
    );
    assert_ne!(
        window.style & ES_MULTILINE_REAL,
        0,
        "the REAL ES_MULTILINE bit (0x0004) must be in the creation style"
    );
    // CW_USEDEFAULT x/y for a CHILD means (0,0) on Windows — notepad relies on
    // the edit starting at the parent's origin.
    assert_eq!(
        (window.x, window.y),
        (0, 0),
        "child CW_USEDEFAULT x/y must be (0,0), not the top-level (100,100)"
    );

    // The real flow shows the windows (notepad's ShowWindow(SW_SHOW) on the
    // main window and the edit) before the message loop; a hidden control
    // must NOT paint — the WM_PAINT dispatch gates on visibility (View >
    // Status Bar regression) — so mirror the show here. SW_SHOW re-arms
    // invalidated, exactly like the ShowWindow handler does.
    for hwnd in [parent, edit] {
        if let Some(window) = state
            .window_state()
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        {
            window.visible = true;
            window.invalidated = true;
        }
    }

    // Type "abc": the caret and the guest's Col stay sane after every char.
    // The guest formats `col + 1` into the status bar, so the 0-based col
    // here is 0 before typing ("Col 1") and tracks the caret afterwards.
    for (ch, expected_col) in [('a', 1_u64), ('b', 2), ('c', 3)] {
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            u64::from(u32::from(ch)),
            0,
        )
        .expect("char ok")
        .expect("some result");
        let col = guest_caret_col(&mut engine, &mut state, edit);
        assert_eq!(
            col, expected_col,
            "Col must track the typed char ({ch}), got {col}"
        );
    }
    assert_eq!(control_text(&state, edit), "abc");
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (3, 3, 3),
        "caret after typing abc"
    );

    // Enter: a multiline EDIT must insert \n and grow the line count.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        crate::user32::VK_RETURN,
        0,
    )
    .expect("enter ok")
    .expect("some result");
    assert!(
        control_text(&state, edit).contains('\n'),
        "Enter must insert \\n in the live multiline EDIT, got {:?}",
        control_text(&state, edit)
    );
    let line_count = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINECOUNT,
        0,
        0,
    )
    .expect("linecount ok")
    .expect("some result");
    assert_eq!(line_count, 2, "EM_GETLINECOUNT must be 2 after Enter");

    // Click mid-line: the caret lands where the pointer is (the 46774a9 path)
    // with a collapsed selection and a sane Col.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_LBUTTONDOWN,
        0,
        mouse_lparam(30, 8),
    )
    .expect("down ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONUP.as_u32(),
        0,
        0,
    )
    .expect("up ok")
    .expect("some result");
    let ui = control_ui(&state, edit);
    assert_eq!(
        ui.sel_start, ui.sel_end,
        "a plain click must not leave a drag selection"
    );
    assert!(
        ui.caret <= control_text(&state, edit).chars().count(),
        "the click caret must stay inside the text"
    );
    let col = guest_caret_col(&mut engine, &mut state, edit);
    assert!(col <= 4, "Col after the click must be sane, got {col}");

    // Paint: the multiline EDIT renders rows top-aligned, not vertically
    // centered (the 9e0e2cc regression, against the live creation style).
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
        .get(&crate::handles::Hwnd::from(parent))
        .expect("published frame")
        .clone();
    let (edit_x, edit_y) = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .map_or((0, 0), |w| (w.x, w.y));
    let frame_width = usize::try_from(frame.width).unwrap_or(0);
    let mut ink_rows: Vec<i32> = Vec::new();
    for y in edit_y.saturating_add(2)..edit_y.saturating_add(300) {
        for x in edit_x.saturating_add(2)..edit_x.saturating_add(300) {
            let idx = (usize::try_from(y).unwrap_or(0))
                .saturating_mul(frame_width)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if frame.pixels.get(idx).copied() == Some(0x0000_0000) {
                ink_rows.push(y);
                break;
            }
        }
    }
    let topmost = ink_rows.first().copied().unwrap_or(0);
    assert!(
        topmost < edit_y.saturating_add(20),
        "the first text row must start at the edit's top edge, got topmost \
         ink row {topmost} vs edit.y {edit_y} (the single-line paint vertically \
         centers the text)"
    );
}
