//! Kernel32 handler tests: critical sections, interlocked ops, error state, locale-aware resource resolution, RegisterWindowMessage, and the mock-data-free handler batch.
use super::*;

// --- Kernel32 ---

#[test]
fn test_critical_section_reenter_single_thread() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // `test_engine` maps [0x1000, 0x101000); place CS there.
    let cs = 0x3000_u64;
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    kernel32::handle_initialize_critical_section(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("init");
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    kernel32::handle_enter_critical_section(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("enter1");
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    kernel32::handle_enter_critical_section(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("enter2");
    let mut rec = [0_u8; 4];
    engine.mem_read(cs + 12, &mut rec).expect("read recursion");
    assert_eq!(u32::from_le_bytes(rec), 2);
    let mut owner = [0_u8; 8];
    engine.mem_read(cs + 16, &mut owner).expect("read owner");
    assert_eq!(u64::from_le_bytes(owner), u64::from(PRIMARY_THREAD_ID));
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    kernel32::handle_leave_critical_section(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("leave1");
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    kernel32::handle_leave_critical_section(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("leave2");
    engine
        .mem_read(cs + 16, &mut owner)
        .expect("read owner unlocked");
    assert_eq!(u64::from_le_bytes(owner), 0);
    assert_eq!(state.kernel.threads.current_tid(), PRIMARY_THREAD_ID);
}

#[test]
fn test_interlocked_ops_host_atomics() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cell = 0x4000_u64;
    // Zero cell.
    engine.mem_write(cell, &0_i32.to_le_bytes()).expect("zero");

    // Increment → 1
    write_regs(&mut engine, cell, 0, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedIncrement")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(rax_low_i32(r.return_value), 1);

    // ExchangeAdd(+5) returns previous 1, cell becomes 6
    write_regs(&mut engine, cell, 5, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedExchangeAdd")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(rax_low_i32(r.return_value), 1);

    // CompareExchange success 6→99
    write_regs(&mut engine, cell, 99, 6, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedCompareExchange")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(rax_low_i32(r.return_value), 6);

    // CompareExchange fail (expect 6, still 99)
    write_regs(&mut engine, cell, 1, 6, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedCompareExchange")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(rax_low_i32(r.return_value), 99);

    let mut bytes = [0_u8; 4];
    engine.mem_read(cell, &mut bytes).expect("read");
    assert_eq!(i32::from_le_bytes(bytes), 99);

    // 64-bit Increment64
    let cell64 = 0x4010_u64;
    engine
        .mem_write(cell64, &10_i64.to_le_bytes())
        .expect("zero64");
    write_regs(&mut engine, cell64, 0, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedIncrement64")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(i64::from_le_bytes(r.return_value.to_le_bytes()), 11);
}

#[test]
fn test_free_library_valid_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x6100_0001, 0, 0, 0, 0);
    assert_return_value!(
        kernel32::handle_free_library(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_free_library_null_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_return_value!(
        kernel32::handle_free_library(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
}

#[test]
fn test_get_last_error() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.last_error = 123;
    let r = kernel32::handle_get_last_error(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetLastError");
    assert_eq!(r.return_value, 123);
}

#[test]
fn test_set_last_error() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.last_error = 0;
    write_regs(&mut engine, 456, 0, 0, 0, 0);
    let _ = kernel32::handle_set_last_error(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetLastError");
    assert_eq!(state.process.last_error, 456);
}

#[test]
fn test_get_startup_info_w_writes_startupinfow() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let info_va = 0x5000;
    // Pre-fill so field writes are observable (STARTUPINFOW is 104 bytes).
    engine
        .mem_write(info_va, &[0xAA_u8; 104])
        .expect("prefill STARTUPINFOW");
    write_regs(&mut engine, info_va, 0, 0, 0, 0);
    // Sentinel return address so the handler's pop is observable (test_engine
    // defaults to 0).
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("kernel32.dll", "GetStartupInfoW")
        .expect("GetStartupInfoW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetStartupInfoW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    // GetStartupInfoW is VOID; RAX mirrors the A variant (unspecified, 0).
    assert_eq!(r.return_value, 0);
    // Mirror of GetStartupInfoA: cb = 104, dwFlags = 0, wShowWindow = 1.
    let mut cb = [0_u8; 4];
    engine.mem_read(info_va, &mut cb).expect("read cb");
    assert_eq!(u32::from_le_bytes(cb), 104);
    let mut flags = [0_u8; 4];
    engine
        .mem_read(info_va + 60, &mut flags)
        .expect("read dwFlags");
    assert_eq!(u32::from_le_bytes(flags), 0);
    let mut show_window = [0_u8; 2];
    engine
        .mem_read(info_va + 64, &mut show_window)
        .expect("read wShowWindow");
    assert_eq!(u16::from_le_bytes(show_window), 1);
    // Windows zero-fills the whole struct: the caller's 0xAA pre-fill must
    // not leak into the untouched fields. Only the cb low byte (offset 0,
    // cb = 104 = 0x68 LE) and the wShowWindow low byte (offset 64, value 1)
    // may be nonzero; dwFlags (60..64) is written as 0.
    let mut full = [0_u8; 104];
    engine
        .mem_read(info_va, &mut full)
        .expect("read full STARTUPINFOW");
    let mut nonzero_offsets: Vec<usize> = Vec::new();
    for (offset, &byte) in full.iter().enumerate() {
        if byte != 0 {
            nonzero_offsets.push(offset);
        }
    }
    assert_eq!(
        nonzero_offsets,
        vec![0, 64],
        "only cb and wShowWindow may be nonzero; the rest must be zeroed"
    );
}

#[test]
fn test_get_user_default_ui_language_returns_lang_id() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // GetUserDefaultUILanguage takes no arguments.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    // Sentinel return address so the handler's pop is observable (test_engine
    // defaults to 0).
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("kernel32.dll", "GetUserDefaultUILanguage")
        .expect("GetUserDefaultUILanguage must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetUserDefaultUILanguage must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    // UI language LANGID comes from the process-wide OnceLock, derived from
    // the host locale (whatever it is on the test machine). The handler reads
    // the same value the locale-aware resolvers use.
    assert_eq!(
        r.return_value,
        u64::from(crate::user32::lang::ui_language()),
        "UI language must be the process-wide host-derived LANGID"
    );
}

// ── Locale-aware resource resolution ────────────────────────────────

/// Push one parsed `RT_STRING` block of `lang` into the main-module table.
fn push_string_block(state: &mut WinApiState, lang: u16, block: u16, strings: &[&str]) {
    use wie_pe::resources::StringBlock;
    let mut slots: [String; 16] = std::array::from_fn(|_| String::new());
    for (i, s) in strings.iter().enumerate() {
        slots[i] = (*s).to_owned();
    }
    state.process.main_module_strings.push(StringBlock {
        block,
        lang,
        strings: slots,
    });
}

/// Seed a notepad-like string block in two locales: German 0x0007 first
/// (resource-directory order — notepad lists 39 locales ascending) and en-US
/// 0x0409 second. "Untitled" lives at id 0x174 (block 24, slot 4) and the
/// file-type filter at id 0x176 (block 24, slot 6).
fn push_locale_string_blocks(state: &mut WinApiState) {
    push_string_block(
        state,
        0x0007,
        24,
        &["", "", "", "", "Unbenannt", "", "Textdateien (*.txt)"],
    );
    push_string_block(
        state,
        0x0409,
        24,
        &["", "", "", "", "Untitled", "", "Text files (*.txt)"],
    );
}

/// Dispatch `LoadStringW` for `id` and return the copied guest buffer.
fn load_string_w(engine: &mut IcedCpu, state: &mut WinApiState, id: u16) -> String {
    let image_base = default_env().image_base;
    let buf = 0x3000;
    write_regs(engine, image_base, u64::from(id), buf, 64, 0);
    dispatch_user32(engine, state, "LoadStringW");
    read_guest_utf16_raw(engine, buf, 64)
}

/// The `(title, filter)` texts the resolution chain must pick for the process
/// UI language: the German block when its primary language is German (exact
/// LANGID or neutral), the en-US block otherwise (exact 0x0409 or the en-US
/// fallback). Keeps the handler-path assertions host-independent.
fn expected_locale_text() -> (&'static str, &'static str) {
    if crate::user32::lang::ui_language() & 0xFF == 0x0007 {
        ("Unbenannt", "Textdateien (*.txt)")
    } else {
        ("Untitled", "Text files (*.txt)")
    }
}

#[test]
fn test_load_string_w_picks_ui_language_locale() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_locale_string_blocks(&mut state);

    // The handler resolves through the process-wide UI language (OnceLock,
    // host-derived), so the en-US block beats the German first block on any
    // non-German host — and the German block on a German host.
    let (title, filter) = expected_locale_text();
    assert_eq!(load_string_w(&mut engine, &mut state, 0x174), title);
    assert_eq!(load_string_w(&mut engine, &mut state, 0x176), filter);
}

#[test]
fn test_load_menu_w_picks_ui_language_locale() {
    use wie_pe::resources::{MenuItemTemplate, MenuTemplate};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Same menu id in two locales: German first, en-US second.
    let template = |lang: u16, text: &str| MenuTemplate {
        id: 0x201,
        lang,
        items: vec![MenuItemTemplate {
            flags: 0x00,
            id: 0x0100,
            text: Some(text.to_owned()),
            sub: Vec::new(),
        }],
    };
    state
        .process
        .main_module_menus
        .push(template(0x0007, "Unbenannt"));
    state
        .process
        .main_module_menus
        .push(template(0x0409, "Untitled"));

    // The 0x0409 template wins over the first (German) block for any
    // non-German UI language; the German one wins on a German host.
    let (title, _) = expected_locale_text();
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_ne!(handle, 0, "known menu id must return a nonzero HMENU");
    let record = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == crate::handles::Hmenu::from(handle))
        .expect("menu record exists");
    assert!(
        matches!(
            record.items.first(),
            Some(crate::user32::menu::MenuEntry::Item { text, .. }) if text == title
        ),
        "UI language must resolve the locale-matching menu template"
    );
}

#[test]
fn test_create_font_indirect_w_resolves_logfontw() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    // W mirror of CreateFontIndirectA: the LOGFONTW face name is UTF-16LE and
    // must survive the round trip through the font record table.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let logfont_va = 0x5000;
    // LOGFONTW header fields (layout shared with LOGFONTA until lfFaceName).
    let height = 16_i32.to_le_bytes();
    engine
        .mem_write(logfont_va, &height)
        .expect("write LOGFONTW.lfHeight");
    let weight = 700_i32.to_le_bytes();
    engine
        .mem_write(logfont_va + 16, &weight)
        .expect("write LOGFONTW.lfWeight");
    let italic_byte = [1_u8];
    engine
        .mem_write(logfont_va + 20, &italic_byte)
        .expect("write LOGFONTW.lfItalic");
    let charset_byte = [1_u8]; // DEFAULT_CHARSET
    engine
        .mem_write(logfont_va + 23, &charset_byte)
        .expect("write LOGFONTW.lfCharSet");
    // lfFaceName is wchar_t[32] at offset 28 (64 bytes, UTF-16LE).
    write_guest_utf16(&mut engine, logfont_va + 28, "Segoe UI");
    write_regs(&mut engine, logfont_va, 0, 0, 0, 0);
    // Sentinel return address so the handler's pop is observable (test_engine
    // defaults to 0).
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("gdi32.dll", "CreateFontIndirectW")
        .expect("CreateFontIndirectW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("CreateFontIndirectW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    // Nonzero HFONT that resolves through the font record table.
    assert_ne!(
        r.return_value, 0,
        "CreateFontIndirectW must return an HFONT"
    );
    let font = state
        .gdi_state()
        .find_font(crate::handles::Hfont::from(r.return_value))
        .expect("returned HFONT must resolve to a font record");
    assert_eq!(font.family, "Segoe UI", "UTF-16 face name round trip");
    assert_eq!(font.height, 16);
    assert_eq!(font.weight, 700);
    assert!(font.italic);
    assert_eq!(font.charset, 1);
}

#[test]
fn test_load_icon_w_make_int_resource_returns_icon() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    // MAKEINTRESOURCEW(id) is a pointer whose high 16 bits are zero; the low
    // word is the resource id. Icons are not parsed yet, so any request
    // resolves to the shared fake icon handle.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // rcx = hinst, rdx = MAKEINTRESOURCEW(0x7F00) → raw value 0x7F00.
    write_regs(&mut engine, 0x1400_0000, 0x7F00, 0, 0, 0);
    // Sentinel return address so the handler's pop is observable (test_engine
    // defaults to 0).
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "LoadIconW")
        .expect("LoadIconW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("LoadIconW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_eq!(
        r.return_value,
        user32::FAKE_ICON_HANDLE,
        "LoadIconW(MAKEINTRESOURCEW) must return the shared fake icon handle"
    );
}

#[test]
fn test_load_cursor_w_make_int_resource_returns_cursor() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    // MAKEINTRESOURCEW(id) is a pointer whose high 16 bits are zero; the low
    // word is the resource id. Cursors are not parsed yet, so any request
    // resolves to the shared fake cursor handle.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // rcx = hinst, rdx = MAKEINTRESOURCEW(0x7F00) → raw value 0x7F00.
    write_regs(&mut engine, 0x1400_0000, 0x7F00, 0, 0, 0);
    // Sentinel return address so the handler's pop is observable (test_engine
    // defaults to 0).
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "LoadCursorW")
        .expect("LoadCursorW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("LoadCursorW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_eq!(
        r.return_value,
        user32::FAKE_CURSOR_HANDLE,
        "LoadCursorW(MAKEINTRESOURCEW) must return the shared fake cursor handle"
    );
}

#[test]
fn test_load_cursor_w_string_name_decodes_utf16() {
    // Full dispatch path for a name-based cursor: rdx points at a UTF-16LE
    // string (not a MAKEINTRESOURCE — the address must have nonzero high 16
    // bits), which the W handler must decode before resolving the cursor.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x1_0000; // high 16 bits nonzero → string, not a resource id
    write_guest_utf16(&mut engine, name_addr, "IDC_ARROW");
    write_regs(&mut engine, 0x1400_0000, name_addr, 0, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "LoadCursorW")
        .expect("LoadCursorW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("LoadCursorW must dispatch");
    assert_eq!(
        r.return_value,
        user32::FAKE_CURSOR_HANDLE,
        "name-based LoadCursorW must resolve to the shared fake cursor handle"
    );
}

#[test]
fn test_load_icon_w_string_name_decodes_utf16() {
    // Full dispatch path for a name-based icon: rdx points at a UTF-16LE
    // string (not a MAKEINTRESOURCE — the address must have nonzero high 16
    // bits), which the W handler must decode before resolving the icon.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x1_0000; // high 16 bits nonzero → string, not a resource id
    write_guest_utf16(&mut engine, name_addr, "IDI_MAIN");
    write_regs(&mut engine, 0x1400_0000, name_addr, 0, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "LoadIconW")
        .expect("LoadIconW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("LoadIconW must dispatch");
    assert_eq!(
        r.return_value,
        user32::FAKE_ICON_HANDLE,
        "name-based LoadIconW must resolve to the shared fake icon handle"
    );
}

/// Write a NUL-terminated UTF-16LE guest string at `addr`.
fn write_guest_utf16(engine: &mut IcedCpu, addr: u64, s: &str) {
    let mut bytes: Vec<u8> = Vec::new();
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine
        .mem_write(addr, &bytes)
        .expect("write guest UTF-16 string");
}

/// Write a NUL-terminated ANSI/UTF-8 guest string at `addr`.
fn write_guest_ansi(engine: &mut IcedCpu, addr: u64, s: &str) {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    engine
        .mem_write(addr, &bytes)
        .expect("write guest ANSI string");
}

/// Run `RegisterWindowMessageA/W` through the full dispatch path and return
/// the handler's return value.
fn register_window_message(
    library: &str,
    name: &str,
    state: &mut WinApiState,
    engine: &mut IcedCpu,
) -> u64 {
    let id = crate::resolve_winapi_id(library, name).expect("must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("RegisterWindowMessage must dispatch");
    r.return_value
}

#[test]
fn test_register_window_message_w_first_id_is_c000() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x5000;
    write_guest_utf16(&mut engine, name_addr, "FINDMSGSTRING");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let value = register_window_message(
        "user32.dll",
        "RegisterWindowMessageW",
        &mut state,
        &mut engine,
    );
    // First registration allocates 0xC000, the start of the reserved range.
    assert_eq!(value, 0xC000);
    assert!(
        (0xC000..=0xFFFF).contains(&value),
        "registered-message id must be in the 0xC000–0xFFFF range, got {value:#x}"
    );
}

#[test]
fn test_register_window_message_same_name_stable_id() {
    // A second call with the same name returns the SAME id (stable per
    // session), including across case variants (Windows matches case-insensitively).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x5000;
    write_guest_utf16(&mut engine, name_addr, "FINDMSGSTRING");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let first = register_window_message(
        "user32.dll",
        "RegisterWindowMessageW",
        &mut state,
        &mut engine,
    );
    write_guest_utf16(&mut engine, name_addr, "findmsgstring");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let second = register_window_message(
        "user32.dll",
        "RegisterWindowMessageW",
        &mut state,
        &mut engine,
    );
    assert_eq!(second, first, "same name must map to the same id");
    // A third name must NOT collide with the registered one.
    write_guest_utf16(&mut engine, name_addr, "MY_PRIVATE_MSG");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let other = register_window_message(
        "user32.dll",
        "RegisterWindowMessageW",
        &mut state,
        &mut engine,
    );
    assert_ne!(other, first, "different names must map to different ids");
}

#[test]
fn test_register_window_message_ids_allocate_sequentially() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x5000;
    for (index, name) in ["MSG_A", "MSG_B", "MSG_C"].iter().enumerate() {
        write_guest_utf16(&mut engine, name_addr, name);
        write_regs(&mut engine, name_addr, 0, 0, 0, 0);
        let value = register_window_message(
            "user32.dll",
            "RegisterWindowMessageW",
            &mut state,
            &mut engine,
        );
        let expected = 0xC000 + index as u64;
        assert_eq!(value, expected, "sequential id allocation");
    }
}

#[test]
fn test_register_window_message_a_and_w_share_cache() {
    // RegisterWindowMessageA and RegisterWindowMessageW with the same name
    // return the SAME id (shared per-session cache).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_addr = 0x5000;
    write_guest_utf16(&mut engine, name_addr, "FINDMSGSTRING");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let wide = register_window_message(
        "user32.dll",
        "RegisterWindowMessageW",
        &mut state,
        &mut engine,
    );
    // A fresh address holds the ANSI copy of the same name.
    write_guest_ansi(&mut engine, name_addr, "FINDMSGSTRING");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let ansi = register_window_message(
        "user32.dll",
        "RegisterWindowMessageA",
        &mut state,
        &mut engine,
    );
    assert_eq!(ansi, wide, "A and W with the same name must share one id");
    // A distinct name still gets the next sequential id — the shared cache
    // must not have consumed an extra slot for the A call.
    write_guest_ansi(&mut engine, name_addr, "OTHER_MSG");
    write_regs(&mut engine, name_addr, 0, 0, 0, 0);
    let other = register_window_message(
        "user32.dll",
        "RegisterWindowMessageA",
        &mut state,
        &mut engine,
    );
    assert_eq!(other, 0xC001, "next distinct name allocates 0xC001");
}

#[test]
fn test_heap_free_double_free_returns_false() {
    let mut engine = test_engine();
    let mut state = winapi_state_default();
    let p = state.heap_state.heap.alloc(64);
    assert_ne!(p, 0);

    write_regs(&mut engine, 0x1, 0, p, 0, 0);
    let r = kernel32::handle_heap_free(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("HeapFree");
    assert_eq!(r.return_value, 1, "first free must succeed");

    state.process.last_error = 0;
    write_regs(&mut engine, 0x1, 0, p, 0, 0);
    let r = kernel32::handle_heap_free(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("HeapFree double");
    assert_eq!(r.return_value, 0, "double free must return FALSE");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

// ── Kernel32: new mock-data-free handlers ─────────────────────────

#[test]
fn test_is_debugger_present() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    assert_return_value!(
        kernel32::handle_is_debugger_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_debug_break() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    assert_return_value!(
        kernel32::handle_debug_break(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_output_debug_string_a() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x3000, 0, 0, 0, STACK_TOP);
    engine.mem_write(0x3000, b"hello\0").ok();
    assert_return_value!(
        kernel32::handle_output_debug_string_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_set_error_mode() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.error_mode = 0;
    write_regs(&mut engine, 0x02, 0, 0, 0, STACK_TOP);
    let r = kernel32::handle_set_error_mode(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetErrorMode");
    // Previous mode was 0.
    assert_eq!(r.return_value, 0);
    assert_eq!(state.process.error_mode, 2);
}

#[test]
fn test_set_thread_error_mode() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.error_mode = 1;
    let prev_va = 0x4000;
    write_regs(&mut engine, 0x03, prev_va, 0, 0, STACK_TOP);
    let r = kernel32::handle_set_thread_error_mode(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetThreadErrorMode");
    assert_eq!(r.return_value, 1); // TRUE
    assert_eq!(state.process.error_mode, 3);
    let mut buf = [0_u8; 4];
    engine.mem_read(prev_va, &mut buf).ok();
    assert_eq!(u32::from_le_bytes(buf), 1); // previous mode written back
}

#[test]
fn test_get_long_path_name_w_returns_input() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let src = 0x3000;
    let dst = 0x4000;
    let units: Vec<u16> = "C:\\test".encode_utf16().collect();
    let mut bytes = Vec::new();
    for u in &units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.push(0);
    bytes.push(0); // NUL terminator
    engine.mem_write(src, &bytes).ok();
    write_regs(&mut engine, src, dst, 260, 0, STACK_TOP);
    let r = kernel32::handle_get_long_path_name_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetLongPathNameW");
    assert_eq!(r.return_value, 7); // "C:\test" = 7 chars
}

#[test]
fn test_create_job_object_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    let r = kernel32::handle_create_job_object_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateJobObjectW");
    assert!(r.return_value != 0);
}

#[test]
fn test_assign_process_to_job_object() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x8000_0001, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        kernel32::handle_assign_process_to_job_object(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_terminate_process() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.kernel.sync.process_dying = false;
    write_regs(&mut engine, 0x8000_0001, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        kernel32::handle_terminate_process(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    assert!(state.kernel.sync.process_dying);
}

#[test]
fn test_open_thread_creates_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1000, 0, 0x5678, 0, STACK_TOP);
    let r = kernel32::handle_open_thread(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("OpenThread");
    assert!(r.return_value != 0);
}

#[test]
fn test_get_file_attributes_ex_w_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // "Not found" needs a concrete root so the probe does not hit the real
    // global app-data bottle. The root need not exist — the probe path does
    // not exist under any root, which is exactly the case under test.
    state.file_io.volumes.bottle_root = Some(std::path::PathBuf::from("/tmp/wie-bottle"));
    let path_va = 0x3000;
    engine
        .mem_write(
            path_va,
            &"C:\\nonexistent"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .ok();
    engine.mem_write(path_va.wrapping_add(26), &[0, 0]).ok();
    write_regs(
        &mut engine,
        path_va,
        1, /* GetFileExInfoStandard */
        0x4000,
        0,
        STACK_TOP,
    );
    let r = kernel32::handle_get_file_attributes_ex_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetFileAttributesExW");
    assert_eq!(r.return_value, 0); // FALSE
    assert_eq!(state.process.last_error, 2); // ERROR_FILE_NOT_FOUND
}

#[test]
fn test_backup_read_invalid_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0x4000, 64, 0x5000, STACK_TOP);
    let r = kernel32::handle_backup_read(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("BackupRead");
    assert_eq!(r.return_value, 0); // FALSE
    assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
}

#[test]
fn test_suspend_thread_invalid_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
    let _r = kernel32::handle_suspend_thread(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SuspendThread");
    assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
}

#[test]
fn test_lock_file_validates_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
    let r = kernel32::handle_lock_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("LockFile");
    assert_eq!(r.return_value, 0); // FALSE — invalid handle
    assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
}

#[test]
fn test_set_file_valid_data_validates_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
    let r = kernel32::handle_set_file_valid_data(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetFileValidData");
    assert_eq!(r.return_value, 0); // FALSE — invalid handle
    assert_eq!(state.process.last_error, 6);
}

/// CreateFileMappingW on an open handle must register a mapping object; a
/// bogus handle must fail with ERROR_INVALID_HANDLE.
#[test]
fn test_create_file_mapping_w_registers_and_validates() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Mount a real host file so CreateFileW can open it with content.
    let host = std::env::temp_dir().join(format!("wie-map-unit-{}.txt", std::process::id()));
    std::fs::write(&host, b"mapped bytes").expect("write host file");
    // Give the test an explicit root so the mount lands under a real bottle
    // rather than the global app-data default.
    let bottle = std::env::temp_dir().join(format!("wie-map-unit-bottle-{}", std::process::id()));
    std::fs::create_dir_all(bottle.join("drive_c")).expect("create bottle");
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(bottle),
        drive_d_root: None,
    };
    kernel32::mount_host_file(&mut state, r"C:\mapped.txt", &host).expect("mount");

    // CreateFileW(C:\mapped.txt) → a valid open-file handle.
    let name_va = 0x3000;
    write_guest_utf16(&mut engine, name_va, r"C:\mapped.txt");
    write_regs(&mut engine, name_va, 0x8000_0000, 0, 0, STACK_TOP); // GENERIC_READ
    engine
        .mem_write(STACK_TOP + 0x28, &3_u32.to_le_bytes())
        .ok(); // OPEN_EXISTING
    let opened = kernel32::handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW");
    let file_handle = opened.return_value;
    assert_ne!(file_handle, u64::MAX, "valid file handle");

    // CreateFileMappingW(hFile, 0, PAGE_READONLY=2, 0, 0, 0): size 0 → file size.
    write_regs(&mut engine, file_handle, 0, 2, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .ok(); // dwMaximumSizeLow
    let mapped = kernel32::handle_create_file_mapping_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileMappingW");
    let mapping_handle = mapped.return_value;
    assert_ne!(mapping_handle, 0, "mapping handle must be non-zero");

    // The mapping object exists in the kernel table and carries the file size.
    let kernel_object = state
        .kernel
        .sync
        .object(mapping_handle)
        .cloned()
        .expect("mapping registered");
    let crate::KernelObject::FileMapping(mapping) = kernel_object else {
        panic!("expected a FileMapping kernel object");
    };
    assert_eq!(mapping.size, 12, "size matches the mounted file's bytes");
    assert_eq!(mapping.guest_path, r"C:\mapped.txt");

    // Bogus source handle → ERROR_INVALID_HANDLE.
    write_regs(&mut engine, 0xDEAD_BEEF, 0, 2, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .ok();
    let failed = kernel32::handle_create_file_mapping_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileMappingW bogus");
    assert_eq!(failed.return_value, 0, "bogus handle → NULL");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");

    let _cleanup = std::fs::remove_file(&host);
}

/// The full read path: MapViewOfFile must copy the mapped file's bytes into a
/// guest region the guest can read, and UnmapViewOfFile must free it.
#[test]
fn test_map_view_of_file_copies_bytes_into_guest_memory() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    let host = std::env::temp_dir().join(format!("wie-mapview-unit-{}.txt", std::process::id()));
    std::fs::write(&host, b"0123456789ab").expect("write host file");
    let bottle =
        std::env::temp_dir().join(format!("wie-mapview-unit-bottle-{}", std::process::id()));
    std::fs::create_dir_all(bottle.join("drive_c")).expect("create bottle");
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(bottle),
        drive_d_root: None,
    };
    kernel32::mount_host_file(&mut state, r"C:\mapped.txt", &host).expect("mount");

    // CreateFileW → handle.
    let name_va = 0x3000;
    write_guest_utf16(&mut engine, name_va, r"C:\mapped.txt");
    write_regs(&mut engine, name_va, 0x8000_0000, 0, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &3_u32.to_le_bytes())
        .ok();
    let opened = kernel32::handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW");
    let file_handle = opened.return_value;

    // CreateFileMappingW → mapping handle.
    write_regs(&mut engine, file_handle, 0, 2, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .ok();
    let mapped = kernel32::handle_create_file_mapping_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileMappingW");
    let mapping_handle = mapped.return_value;

    // MapViewOfFile(mapping, FILE_MAP_READ=4, offsetHigh=0, offsetLow=0, 0=whole file).
    write_regs(&mut engine, mapping_handle, 4, 0, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .ok(); // dwNumberOfBytesToMap
    let view = kernel32::handle_map_view_of_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("MapViewOfFile");
    let view_va = view.return_value;
    assert_ne!(view_va, 0, "view must be a guest VA");

    // The guest reads the mapped file's bytes at view_va.
    let mut read_back = [0_u8; 12];
    engine
        .mem_read(view_va, &mut read_back)
        .expect("read mapped view");
    assert_eq!(
        &read_back, b"0123456789ab",
        "bytes copied into guest memory"
    );

    // A partial map at an offset reads the tail.
    write_regs(&mut engine, mapping_handle, 4, 0, 4, STACK_TOP); // offset 4
    engine
        .mem_write(STACK_TOP + 0x28, &4_u64.to_le_bytes())
        .ok(); // 4 bytes
    let view2 = kernel32::handle_map_view_of_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("MapViewOfFile partial");
    let view2_va = view2.return_value;
    assert_ne!(view2_va, 0);
    let mut tail = [0_u8; 4];
    engine
        .mem_read(view2_va, &mut tail)
        .expect("read partial view");
    assert_eq!(&tail, b"4567", "offset + length respected");

    // UnmapViewOfFile frees both views.
    write_regs(&mut engine, view_va, 0, 0, 0, STACK_TOP);
    kernel32::handle_unmap_view_of_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("UnmapViewOfFile");
    write_regs(&mut engine, view2_va, 0, 0, 0, STACK_TOP);
    kernel32::handle_unmap_view_of_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("UnmapViewOfFile second");

    // CloseHandle on the mapping removes the kernel object.
    write_regs(&mut engine, mapping_handle, 0, 0, 0, STACK_TOP);
    kernel32::handle_close_handle(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CloseHandle mapping");
    assert!(
        state.kernel.sync.object(mapping_handle).is_none(),
        "CloseHandle removes the mapping object"
    );

    let _cleanup = std::fs::remove_file(&host);
}

/// The global-bottle policy, end to end at the handler level: a file op with
/// NO `--root` and NO `WIE_ROOT` succeeds — guest `C:\…` maps to the default
/// app-data bottle, which the op creates on demand.
#[test]
fn test_file_op_without_root_creates_and_writes_the_global_bottle() {
    let mut engine = test_engine();
    // `default_winapi_state` uses `VolumeConfig::default()`: no override, so
    // resolution falls back to the global app-data bottle.
    let mut state = default_winapi_state();

    let unique = format!("global-bottle-{}.txt", std::process::id());
    let guest_path = format!(r"C:\wie-global-bottle-e2e\{unique}");
    let name_va = 0x3000;
    write_guest_utf16(&mut engine, name_va, &guest_path);
    // CreateFileW(ptr, GENERIC_WRITE=0x40000000, 0, 0, CREATE_ALWAYS=2, ...).
    write_regs(&mut engine, name_va, 0x4000_0000, 0, 0, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &2_u32.to_le_bytes())
        .ok();
    let created = kernel32::handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW without a root must not error");
    let file_handle = created.return_value;
    assert_ne!(
        file_handle,
        u64::MAX,
        "valid file handle via the global bottle"
    );

    // WriteFile(h, buf, 5, &written, 0).
    const DATA: &[u8] = b"GLOBAL";
    engine.mem_write(0x5000, DATA).expect("stage write buffer");
    write_regs(
        &mut engine,
        file_handle,
        0x5000,
        DATA.len() as u64,
        0x6000,
        STACK_TOP,
    );
    let wrote = kernel32::handle_write_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("WriteFile");
    assert_eq!(wrote.return_value, 1, "WriteFile succeeds");
    let mut written = [0_u8; 4];
    engine
        .mem_read(0x6000, &mut written)
        .expect("read written count");
    assert_eq!(u32::from_le_bytes(written), DATA.len() as u32);

    write_regs(&mut engine, file_handle, 0, 0, 0, STACK_TOP);
    kernel32::handle_close_handle(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CloseHandle");

    // The global bottle now exists and holds the written bytes.
    let host = crate::vfs::global_bottle_root()
        .join("drive_c")
        .join("wie-global-bottle-e2e")
        .join(&unique);
    assert!(
        host.is_file(),
        "file must exist under the global bottle: {}",
        host.display()
    );
    assert_eq!(
        std::fs::read(&host).expect("read back"),
        DATA,
        "bytes round-trip through the global bottle"
    );

    let _cleanup = std::fs::remove_file(&host);
}

/// Dispatch one fixed-dir handler (`GetWindowsDirectory*`, `GetSystemDirectory*`,
/// `GetTempPath*`) and return its return value.
fn dispatch_fixed_dir_handler(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let result = {
        let mut ctx = HandlerContext::new(engine, default_env(), state);
        match name {
            "GetWindowsDirectoryW" => kernel32::handle_get_windows_directory_w(&mut ctx),
            "GetWindowsDirectoryA" => kernel32::handle_get_windows_directory_a(&mut ctx),
            "GetSystemDirectoryW" => kernel32::handle_get_system_directory_w(&mut ctx),
            "GetSystemDirectoryA" => kernel32::handle_get_system_directory_a(&mut ctx),
            "GetTempPathW" => kernel32::handle_get_temp_path_w(&mut ctx),
            "GetTempPathA" => kernel32::handle_get_temp_path_a(&mut ctx),
            other => panic!("unknown fixed-dir handler {other}"),
        }
    }
    .expect("fixed-dir handler must dispatch");
    result.return_value
}

/// The path-returning kernel32 handlers must point INTO the seeded default
/// skeleton: after a temp-root bottle is seeded, every returned directory
/// exists on the host under `{root}/drive_c/…`.
#[test]
fn test_windows_system_and_temp_dir_handlers_return_seeded_paths() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed a fresh temp-root bottle so the fixed dirs exist on the host.
    let root = std::env::temp_dir().join(format!("wie-fixed-dirs-{}", std::process::id()));
    let _unused = std::fs::remove_dir_all(&root);
    crate::vfs::seed_default_skeleton(&root).expect("seed skeleton");
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(root.clone()),
        drive_d_root: None,
    };

    // GetTempPath* appends a trailing backslash (Microsoft Learn).
    let cases: &[(&str, u64, &str)] = &[
        ("GetWindowsDirectoryW", 0x6000, r"C:\Windows"),
        ("GetWindowsDirectoryA", 0x6100, r"C:\Windows"),
        ("GetSystemDirectoryW", 0x6200, r"C:\Windows\System32"),
        ("GetSystemDirectoryA", 0x6300, r"C:\Windows\System32"),
        ("GetTempPathW", 0x6400, r"C:\Users\WIE\AppData\Local\Temp\"),
        ("GetTempPathA", 0x6500, r"C:\Users\WIE\AppData\Local\Temp\"),
    ];
    for &(name, buf, expected) in cases {
        // rcx = buffer length in TCHARs, rdx = buffer.
        write_regs(&mut engine, 260, buf, 0, 0, STACK_TOP);
        let len = dispatch_fixed_dir_handler(&mut engine, &mut state, name);
        assert!(len > 0, "{name} must return a length");
        let returned = if name.ends_with('W') {
            read_guest_utf16_raw(&mut engine, buf, 260)
        } else {
            read_guest_ansi_raw(&mut engine, buf, 260)
        };
        assert_eq!(returned, expected, "{name} must return the seeded dir");
        // The returned path maps into the seeded skeleton and exists on disk.
        let map =
            crate::vfs::guest_path_to_host(&state.file_io.volumes, returned.trim_end_matches('\\'))
                .unwrap_or_else(|| panic!("{name}: returned path must map into the bottle"));
        assert!(
            map.host.is_dir(),
            "{name}: {} must exist on the host",
            map.host.display()
        );
    }
    let _unused = std::fs::remove_dir_all(&root);
}
