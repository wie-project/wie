//! Unit tests for WinAPI handler dispatch and the state types.
//!
//! Moved wholesale from `lib.rs` when the state definitions were extracted
//! into this module. `use super::*` resolves the state types; the crate-root
//! re-exports keep every `crate::X` path inside the tests unchanged.
#![allow(clippy::expect_used)]

use super::*;
use wie_cpu::{CpuEngine, IcedCpu};

use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::{Arc, Mutex};

use crate::guest_heap::GuestHeap;
use crate::sync_obj::SyncState;
use crate::thread::{PRIMARY_THREAD_ID, ThreadState};
use crate::vfs::VolumeConfig;
use crate::{
    advapi32, comctl32, comdlg32, d3d9, dll_loader, gdi32, kernel32, oleaut32, present, shell32,
    user32,
};

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
// STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
const STACK_TOP: u64 = 0x100_FF00;

/// Minimal engine for handler unit tests: maps guest pages with a valid return address on the stack.
fn test_engine() -> IcedCpu {
    let mut cpu = IcedCpu::open_x86_64();
    cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
        .expect("map test memory");
    cpu.mem_map(STACK_VA, STACK_SIZE, wie_cpu::RwxPerms::ALL)
        .expect("map test stack");
    // Write a dummy return address — every handler calls return_from_win64_api which reads it.
    cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
        .expect("write return address");
    cpu.write_rsp(STACK_TOP).ok();
    cpu
}

fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64, rsp: u64) {
    cpu.write_rcx(rcx).ok();
    cpu.write_rdx(rdx).ok();
    cpu.write_r8(r8).ok();
    cpu.write_r9(r9).ok();
    cpu.write_rsp(if rsp == 0 { STACK_TOP } else { rsp }).ok();
}

fn default_env() -> WinApiEnvironment {
    WinApiEnvironment {
        image_base: 0x0000_0000_1400_0000,
        command_line_a_ptr: 0,
        command_line_w_ptr: 0,
        environment_strings_w_ptr: 0,
        module_file_name_a_ptr: 0,
        module_file_name_w_ptr: 0,
        process_heap_handle: 1,
    }
}

/// Low 32 bits of RAX as signed LONG (Win64 return convention for Interlocked*).
fn rax_low_i32(rax: u64) -> i32 {
    i32::from_le_bytes(u32::try_from(rax & 0xffff_ffff).unwrap_or(0).to_le_bytes())
}

fn default_winapi_state() -> WinApiState {
    // Simplified default with a bump heap covering [0x2000, 0x10000).
    let mut heap = GuestHeap::new(0x2000, 0x10000);
    heap.attach_guest_control(0x2000);
    WinApiState {
        heap_state: HeapState {
            heap,
            ..winapi_state_default().heap_state
        },
        ..winapi_state_default()
    }
}

fn winapi_state_default() -> WinApiState {
    // This must stay in sync with the fields of WinApiState.
    // Only the heap is customised; everything else is default.
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
            stdin_mode: GuestStdinMode::InjectOnly,
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
        message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
        module_state: ModuleState {
            loaded_modules: HashMap::new(),
            import_resolver: None,
            get_proc_address_cache: HashMap::new(),
            next_module_handle: crate::ModuleHandle::from(dll_loader::REAL_MODULE_HANDLE_BASE),
        },
    }
}

/// All-zero environment for handlers that don't read it.
fn test_environment() -> WinApiEnvironment {
    WinApiEnvironment {
        image_base: 0,
        command_line_a_ptr: 0,
        command_line_w_ptr: 0,
        environment_strings_w_ptr: 0,
        module_file_name_a_ptr: 0,
        module_file_name_w_ptr: 0,
        process_heap_handle: 0,
    }
}

macro_rules! assert_return_value {
    ($result:expr, $expected:expr) => {
        let r = $result.expect("handler should succeed");
        assert_eq!(r.return_value, $expected, "return value mismatch");
    };
}

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
    let info_ptr = 0x5000;
    // Pre-fill so field writes are observable (STARTUPINFOW is 104 bytes).
    engine
        .mem_write(info_ptr, &[0xAA_u8; 104])
        .expect("prefill STARTUPINFOW");
    write_regs(&mut engine, info_ptr, 0, 0, 0, 0);
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
    engine.mem_read(info_ptr, &mut cb).expect("read cb");
    assert_eq!(u32::from_le_bytes(cb), 104);
    let mut flags = [0_u8; 4];
    engine
        .mem_read(info_ptr + 60, &mut flags)
        .expect("read dwFlags");
    assert_eq!(u32::from_le_bytes(flags), 0);
    let mut show_window = [0_u8; 2];
    engine
        .mem_read(info_ptr + 64, &mut show_window)
        .expect("read wShowWindow");
    assert_eq!(u16::from_le_bytes(show_window), 1);
    // Windows zero-fills the whole struct: the caller's 0xAA pre-fill must
    // not leak into the untouched fields. Only the cb low byte (offset 0,
    // cb = 104 = 0x68 LE) and the wShowWindow low byte (offset 64, value 1)
    // may be nonzero; dwFlags (60..64) is written as 0.
    let mut full = [0_u8; 104];
    engine
        .mem_read(info_ptr, &mut full)
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
    let logfont_ptr = 0x5000;
    // LOGFONTW header fields (layout shared with LOGFONTA until lfFaceName).
    let height = 16_i32.to_le_bytes();
    engine
        .mem_write(logfont_ptr, &height)
        .expect("write LOGFONTW.lfHeight");
    let weight = 700_i32.to_le_bytes();
    engine
        .mem_write(logfont_ptr + 16, &weight)
        .expect("write LOGFONTW.lfWeight");
    let italic_byte = [1_u8];
    engine
        .mem_write(logfont_ptr + 20, &italic_byte)
        .expect("write LOGFONTW.lfItalic");
    let charset_byte = [1_u8]; // DEFAULT_CHARSET
    engine
        .mem_write(logfont_ptr + 23, &charset_byte)
        .expect("write LOGFONTW.lfCharSet");
    // lfFaceName is wchar_t[32] at offset 28 (64 bytes, UTF-16LE).
    write_guest_utf16(&mut engine, logfont_ptr + 28, "Segoe UI");
    write_regs(&mut engine, logfont_ptr, 0, 0, 0, 0);
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

// --- User32 ---

#[test]
fn test_get_async_key_state_default() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // VK_RETURN = 0x0D, keyboard_state starts all zero.
    write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_async_key_state_down() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // VK_RETURN high bit set — index is a compile-time constant in bounds.
    state.window_state().keyboard_state.set(0x0D, 0x80);
    write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x81
    );
}

#[test]
fn test_keyboard_state_shift_update_path() {
    // Mirrors the host input seam (app.rs set_key_state): pressing the
    // Shift key sets bit 0x80 on VK_SHIFT (0x10) and releasing clears it,
    // which is exactly what IsDialogMessage's Shift+Tab and
    // GetAsyncKeyState/GetKeyState read.
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Press: app.rs set_key_state(0x10, true).
    let held = state.window_state().keyboard_state.get(0x10) | 0x80;
    state.window_state().keyboard_state.set(0x10, held);
    write_regs(&mut engine, 0x10, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x81
    );

    // Release: app.rs set_key_state(0x10, false).
    let released = state.window_state().keyboard_state.get(0x10) & !0x80;
    state.window_state().keyboard_state.set(0x10, released);
    write_regs(&mut engine, 0x10, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_sys_color_highlight_is_not_bgr_swapped() {
    // COLOR_HIGHLIGHT (13) and COLOR_ACTIVECAPTION (2) are #0078D7 stored
    // as 0RGB — the old 0xD77830 was the B/R-swapped value (rendered
    // orange).
    for index in [13_u64, 2] {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, index, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_sys_color(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0x0000_78D7
        );
    }
}

#[test]
fn test_get_sys_color_3d_edge_colors() {
    // COLOR_BTNHIGHLIGHT (20) = white; COLOR_3DDKSHADOW (21) = dark gray
    // (both previously fell through to the BTNFACE fallback).
    let cases = [(20_u64, 0x00FF_FFFF_u64), (21, 0x0069_6969)];
    for (index, expected) in cases {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, index, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_sys_color(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            expected
        );
    }
}

#[test]
fn test_peek_message_a_empty_queue() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Write a valid MSG struct address (doesn't matter since queue is empty).
    write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_peek_message_a_with_message() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    // Map memory for the MSG struct.
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // Push a WM_PAINT message for any window.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x100),
            message: 15, // WM_PAINT
            word_parameter: 0,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // PeekMessageA(msg_ptr=msg_va, hwnd=0, min=0, max=0, wRemoveMsg=1)
    // wRemoveMsg is on the stack at RSP+0x28.
    write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
    // Write wRemoveMsg=1 (PM_REMOVE) at RSP+0x28.
    engine.mem_write(0x3028, &1_u32.to_le_bytes()).ok();
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // WM_PAINT should have been removed from the queue.
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        0
    );
}

#[test]
fn test_peek_message_a_noremove() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x100),
            message: 15,
            word_parameter: 0,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
    // wRemoveMsg=0 (PM_NOREMOVE) at RSP+0x28.
    engine.mem_write(0x3028, &0_u32.to_le_bytes()).ok();
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // Message should still be in the queue.
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        1
    );
}

#[test]
fn test_get_message_wm_quit_bypasses_window_filter() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // WM_QUIT addressed to a window that does NOT match the filter.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x1234),
            message: 0x12, // WM_QUIT
            word_parameter: 7,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // GetMessageA(msg_ptr, hWnd=0x5678 filter, min=0, max=0).
    write_regs(&mut engine, msg_va, 0x5678, 0, 0, 0x3000);
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    // GetMessage returns 0 (FALSE) for WM_QUIT regardless of the filter.
    assert_eq!(r.return_value, 0);
    // The returned MSG carries the WM_QUIT and its wParam.
    let mut bytes = [0_u8; 4];
    engine
        .mem_read(msg_va + 8, &mut bytes)
        .expect("read MSG.message");
    assert_eq!(u32::from_le_bytes(bytes), 0x12);
    engine
        .mem_read(msg_va + 16, &mut bytes)
        .expect("read MSG.wParam");
    assert_eq!(u32::from_le_bytes(bytes), 7);
}

#[test]
fn test_get_message_dialog_filter_matches_descendants() {
    use crate::QueuedWindowMessage;
    use crate::WindowRecord;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // Owner (parentless) → dialog (parent = owner) → button (parent = dialog).
    let owner = 0x6610_0001_u64;
    let dialog = 0x6610_0002_u64;
    let child = 0x6610_0003_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(owner),
        title: "Owner".to_owned(),
        width: 100,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(dialog),
        parent_handle: crate::handles::Hwnd::from(owner),
        title: "Dialog".to_owned(),
        dialog_proc: 0x7000_0001,
        width: 80,
        height: 60,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(child),
        parent_handle: crate::handles::Hwnd::from(dialog),
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        control_text: "OK".to_owned(),
        menu_handle: 1,
        visible: true,
        width: 40,
        height: 20,
        ..Default::default()
    });
    // A message addressed to the button inside the dialog.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(child),
            message: 0x0100, // WM_KEYDOWN
            word_parameter: 0x09,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // GetMessageA(msg_ptr, hWnd=dialog, min=0, max=0): the child's message
    // must match through the dialog's descendant rule.
    write_regs(&mut engine, msg_va, dialog, 0, 0, 0x3000);
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    assert_eq!(r.return_value, 1);
    let mut bytes = [0_u8; 8];
    engine.mem_read(msg_va, &mut bytes).expect("read MSG.hwnd");
    assert_eq!(u64::from_le_bytes(bytes), child);
}

#[test]
fn test_empty_queue_yields_while_dialog_open_under_exit_on_idle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().message_queue_idle_policy = MessageQueueIdlePolicy::ExitOnIdle;
    // An open modal dialog must never see the synthetic regression WM_QUIT.
    state.lock_message_queue().dialog_depth = 1;
    write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
    let result = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ));
    assert!(
        result.is_err(),
        "empty queue under an open dialog must yield"
    );
    let error = result.expect_err("expected the WaitingForMessage signal");
    assert!(
        error
            .downcast_ref::<WinApiControlSignal>()
            .is_some_and(|signal| matches!(signal, WinApiControlSignal::WaitingForMessage)),
        "expected WaitingForMessage, got {error:?}"
    );
    // No WM_QUIT was synthesized into the queue.
    assert!(
        state
            .lock_message_queue()
            .messages
            .iter()
            .all(|m| m.message != 0x12)
    );
    // With no dialog open, ExitOnIdle still synthesizes WM_QUIT (regression
    // path) and GetMessage returns 0.
    state.lock_message_queue().dialog_depth = 0;
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    assert_eq!(r.return_value, 0);
}

#[test]
fn test_get_window_text_length_w_reports_text_length() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "Hello".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(
        r.return_value, 5,
        "\"Hello\" is 5 UTF-16 units excluding the NUL"
    );
}

#[test]
fn test_get_window_text_length_w_empty_is_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(r.return_value, 0, "empty title must report length 0");
}

#[test]
fn test_get_window_text_length_w_unknown_hwnd_is_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(r.return_value, 0, "unknown hwnd must report length 0");
}

#[test]
fn test_get_window_text_length_a_matches_ascii() {
    // ANSI mirror: for ASCII text the byte count equals the UTF-16 unit count.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "Hello".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthA")
        .expect("GetWindowTextLengthA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthA must dispatch");
    assert_eq!(
        r.return_value, 5,
        "ASCII \"Hello\" is 5 ANSI chars excluding the NUL"
    );
}

#[test]
fn test_get_window_text_length_a_counts_cp1252_chars() {
    // Windows ANSI length counts CP1252 characters, not UTF-8 bytes:
    // "café" is 5 UTF-8 bytes but 4 CP1252 chars (é encodes to one byte).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "café".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthA")
        .expect("GetWindowTextLengthA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthA must dispatch");
    assert_eq!(
        r.return_value, 4,
        "\"café\" is 4 CP1252 chars, not 5 UTF-8 bytes"
    );
}

// --- GetWindowPlacement / SetWindowPlacement ---

/// Push a known window record with placement geometry and return its handle.
fn push_geometry_window(state: &mut WinApiState) -> u64 {
    let handle = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(handle),
        title: "Placement".to_owned(),
        x: 40,
        y: 50,
        width: 600,
        height: 400,
        visible: true,
        ..Default::default()
    });
    handle
}

/// Read a guest u32 at `addr` (test helper mirroring `read_guest_u32`).
fn read_test_u32(engine: &mut IcedCpu, addr: u64) -> u32 {
    let mut bytes = [0_u8; 4];
    engine.mem_read(addr, &mut bytes).expect("read guest u32");
    u32::from_le_bytes(bytes)
}

/// Write a full WINDOWPLACEMENT struct for `SetWindowPlacement` tests.
fn write_placement_struct(
    engine: &mut IcedCpu,
    ptr: u64,
    show_cmd: u32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) {
    engine
        .mem_write(ptr, &user32::WINDOWPLACEMENT_LENGTH.to_le_bytes())
        .expect("placement length");
    engine
        .mem_write(ptr + 4, &0_u32.to_le_bytes())
        .expect("placement flags");
    engine
        .mem_write(ptr + 8, &show_cmd.to_le_bytes())
        .expect("placement showCmd");
    for offset in [12, 16, 20, 24] {
        engine
            .mem_write(ptr + offset, &0_i32.to_le_bytes())
            .expect("placement point");
    }
    for (offset, value) in [(28, left), (32, top), (36, right), (40, bottom)] {
        engine
            .mem_write(ptr + offset, &value.to_le_bytes())
            .expect("placement rect field");
    }
}

/// Dispatch `SetWindowPlacement` through the full name→id→handler path.
fn dispatch_set_window_placement(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    hwnd: u64,
    placement_ptr: u64,
) -> u64 {
    write_regs(engine, hwnd, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "SetWindowPlacement")
        .expect("SetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("SetWindowPlacement must dispatch");
    r.return_value
}

/// Read a guest i32 at `addr` (test helper mirroring `read_guest_i32`).
fn read_test_i32(engine: &mut IcedCpu, addr: u64) -> i32 {
    let mut bytes = [0_u8; 4];
    engine.mem_read(addr, &mut bytes).expect("read guest i32");
    i32::from_le_bytes(bytes)
}

#[test]
fn test_get_window_placement_fills_struct() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    write_regs(&mut engine, hwnd, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowPlacement")
        .expect("GetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 1, "known hwnd must return TRUE");

    // WINDOWPLACEMENT (x64): UINT length @0, UINT flags @4, UINT showCmd @8,
    // POINT ptMinPosition @12, POINT ptMaxPosition @20, RECT rcNormalPosition @28.
    assert_eq!(
        read_test_u32(&mut engine, placement_ptr),
        user32::WINDOWPLACEMENT_LENGTH,
        "length must be sizeof(WINDOWPLACEMENT)"
    );
    assert_eq!(
        read_test_u32(&mut engine, placement_ptr + 4),
        0,
        "flags must be 0"
    );
    assert_eq!(
        read_test_u32(&mut engine, placement_ptr + 8),
        1,
        "visible window reports SW_SHOWNORMAL"
    );
    assert_eq!(
        read_test_i32(&mut engine, placement_ptr + 28),
        40,
        "rcNormalPosition.left comes from the window rect"
    );
    assert_eq!(
        read_test_i32(&mut engine, placement_ptr + 32),
        50,
        "rcNormalPosition.top comes from the window rect"
    );
    assert_eq!(
        read_test_i32(&mut engine, placement_ptr + 36),
        640,
        "rcNormalPosition.right = x + width"
    );
    assert_eq!(
        read_test_i32(&mut engine, placement_ptr + 40),
        450,
        "rcNormalPosition.bottom = y + height"
    );
}

#[test]
fn test_get_window_placement_hidden_window_reports_sw_hide() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let handle = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(handle),
        visible: false,
        ..Default::default()
    });
    let placement_ptr = 0x4000_u64;
    write_regs(&mut engine, handle, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowPlacement")
        .expect("GetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 1, "known hwnd must return TRUE");
    assert_eq!(
        read_test_u32(&mut engine, placement_ptr + 8),
        0,
        "hidden window reports SW_HIDE"
    );
}

#[test]
fn test_get_window_placement_unknown_hwnd_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234, 0x4000, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowPlacement")
        .expect("GetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 0, "unknown hwnd must return FALSE");
}

#[test]
fn test_get_window_placement_null_ptr_is_false() {
    // The other cell of the {NULL ptr, unknown hwnd, valid} × {Get, Set}
    // matrix: a known hwnd with a NULL placement pointer must return FALSE
    // and must not write through the NULL pointer.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    write_regs(&mut engine, hwnd, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowPlacement")
        .expect("GetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowPlacement must dispatch");
    assert_eq!(
        r.return_value, 0,
        "NULL placement pointer must return FALSE"
    );
}

#[test]
fn test_set_window_placement_stores_placement() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    // length, flags, showCmd=SW_SHOWMINIMIZED, ptMinPosition, ptMaxPosition,
    // then rcNormalPosition (20, 30, 220, 130).
    engine
        .mem_write(placement_ptr, &user32::WINDOWPLACEMENT_LENGTH.to_le_bytes())
        .expect("placement length");
    engine
        .mem_write(placement_ptr + 4, &0_u32.to_le_bytes())
        .expect("placement flags");
    engine
        .mem_write(placement_ptr + 8, &2_u32.to_le_bytes())
        .expect("placement showCmd");
    for offset in [12, 16, 20, 24] {
        engine
            .mem_write(placement_ptr + offset, &0_i32.to_le_bytes())
            .expect("placement point");
    }
    engine
        .mem_write(placement_ptr + 28, &20_i32.to_le_bytes())
        .expect("placement left");
    engine
        .mem_write(placement_ptr + 32, &30_i32.to_le_bytes())
        .expect("placement top");
    engine
        .mem_write(placement_ptr + 36, &220_i32.to_le_bytes())
        .expect("placement right");
    engine
        .mem_write(placement_ptr + 40, &130_i32.to_le_bytes())
        .expect("placement bottom");

    write_regs(&mut engine, hwnd, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "SetWindowPlacement")
        .expect("SetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("SetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 1, "known hwnd must return TRUE");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record must still exist");
    assert_eq!(window.x, 20, "rcNormalPosition.left must be stored");
    assert_eq!(window.y, 30, "rcNormalPosition.top must be stored");
    assert_eq!(window.width, 200, "width = right - left");
    assert_eq!(window.height, 100, "height = bottom - top");
    assert!(window.visible, "nonzero showCmd shows the window");
}

#[test]
fn test_set_window_placement_sw_hide_hides_window() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    engine
        .mem_write(placement_ptr, &user32::WINDOWPLACEMENT_LENGTH.to_le_bytes())
        .expect("placement length");
    engine
        .mem_write(placement_ptr + 4, &0_u32.to_le_bytes())
        .expect("placement flags");
    // SW_HIDE (0): keep the previous rect, hide the window.
    engine
        .mem_write(placement_ptr + 8, &0_u32.to_le_bytes())
        .expect("placement showCmd");
    for offset in [12, 16, 20, 24, 28, 32, 36, 40] {
        engine
            .mem_write(placement_ptr + offset, &0_i32.to_le_bytes())
            .expect("placement field");
    }
    write_regs(&mut engine, hwnd, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "SetWindowPlacement")
        .expect("SetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("SetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 1, "known hwnd must return TRUE");
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record must still exist");
    assert!(!window.visible, "SW_HIDE must hide the window");
}

/// Register a class with the classic `(HBRUSH)(COLOR_WINDOW + 1)` stock brush
/// (6 = COLOR_WINDOW + 1) and create a top-level window (control_kind = None)
/// from it — notepad's main-window pattern: created without `WS_VISIBLE`,
/// shown later via `SetWindowPlacement`.
fn push_placement_brush_window(state: &mut WinApiState, class_name: &str) -> u64 {
    let atom = crate::user32::register_window_class(
        state,
        WindowClassRecord {
            atom: 0,
            class_name: class_name.to_owned(),
            window_proc: 0x7000_0000,
            style: 0,
            instance_handle: 0,
            icon_handle: 0,
            cursor_handle: 0,
            background_brush: 6,
            small_icon_handle: 0,
            menu_name: 0,
            unicode: true,
        },
    )
    .expect("register class");
    assert_ne!(atom, 0, "class registration must succeed");

    let (hwnd, _, _) = crate::user32::create_window_record(
        state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name(class_name.to_owned()),
            title: "Untitled - Notepad".to_owned(),
            style: 0,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        },
        true,
    )
    .expect("create window");
    assert_ne!(hwnd, 0, "create_window_record must allocate a handle");
    hwnd
}

#[test]
fn test_set_window_placement_sw_shownormal_triggers_erase_paint_cycle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_placement_brush_window(&mut state, "PlacementBrush");
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
            .expect("created window exists")
            .invalidated,
        "a hidden top-level window is not invalidated at creation"
    );

    // SW_SHOWNORMAL: the placement call is what shows the window, so it must
    // enter the erase/paint cycle exactly like ShowWindow(SW_SHOW).
    let placement_ptr = 0x4000_u64;
    write_placement_struct(&mut engine, placement_ptr, 1, 20, 30, 220, 130);
    let returned = dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr);
    assert_eq!(returned, 1, "known hwnd must return TRUE");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record must still exist");
    assert!(window.visible, "SW_SHOWNORMAL shows the window");
    assert!(
        window.invalidated,
        "showing via SetWindowPlacement must invalidate the window"
    );
    assert!(
        window.flags.contains(WindowFlags::ERASE_BACKGROUND),
        "the first paint after SetWindowPlacement must erase the class brush"
    );
}

#[test]
fn test_set_window_placement_sw_hide_skips_erase_invalidation() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_placement_brush_window(&mut state, "PlacementHide");

    // SW_HIDE (0): a hidden window never enters the erase/paint cycle.
    let placement_ptr = 0x4000_u64;
    write_placement_struct(&mut engine, placement_ptr, 0, 20, 30, 220, 130);
    let returned = dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr);
    assert_eq!(returned, 1, "known hwnd must return TRUE");

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record must still exist");
    assert!(!window.visible, "SW_HIDE must hide the window");
    assert!(
        !window.invalidated,
        "hiding must not invalidate (hidden windows don't paint)"
    );
    assert!(
        !window.flags.contains(WindowFlags::ERASE_BACKGROUND),
        "hiding must not request an erase"
    );
}

#[test]
fn test_set_window_placement_short_length_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    // Pre-44 length (a 32-bit WINDOWPLACEMENT); the record must be untouched.
    engine
        .mem_write(placement_ptr, &40_u32.to_le_bytes())
        .expect("placement length");
    write_regs(&mut engine, hwnd, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "SetWindowPlacement")
        .expect("SetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("SetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 0, "length < 44 must return FALSE");
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record must still exist");
    assert_eq!(window.x, 40, "short length must not move the window");
}

#[test]
fn test_set_window_placement_unknown_hwnd_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let placement_ptr = 0x4000_u64;
    engine
        .mem_write(placement_ptr, &44_u32.to_le_bytes())
        .expect("placement length");
    write_regs(&mut engine, 0x1234, placement_ptr, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "SetWindowPlacement")
        .expect("SetWindowPlacement must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("SetWindowPlacement must dispatch");
    assert_eq!(r.return_value, 0, "unknown hwnd must return FALSE");
}

#[test]
fn test_set_window_placement_requests_host_geometry_on_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    write_placement_struct(&mut engine, placement_ptr, 2, 20, 30, 220, 130);
    assert_eq!(
        dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr),
        1,
        "known hwnd must return TRUE"
    );
    assert_eq!(
        state.window_state().host_geometry_request,
        Some((20, 30, 200, 100)),
        "a changed rcNormalPosition must be forwarded to the host window"
    );
}

#[test]
fn test_set_window_placement_unchanged_rect_skips_host_sync() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // push_geometry_window's record is already at (40, 50, 600, 400).
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;
    write_placement_struct(&mut engine, placement_ptr, 1, 40, 50, 640, 450);
    assert_eq!(
        dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr),
        1,
        "known hwnd must return TRUE"
    );
    assert_eq!(
        state.window_state().host_geometry_request,
        None,
        "an unchanged rect must not move the host window"
    );
}

#[test]
fn test_set_window_placement_fake_window_forwards_host_geometry() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let placement_ptr = 0x4000_u64;
    write_placement_struct(&mut engine, placement_ptr, 1, 100, 200, 300, 300);
    assert_eq!(
        dispatch_set_window_placement(
            &mut engine,
            &mut state,
            user32::FAKE_WINDOW_HANDLE,
            placement_ptr,
        ),
        1,
        "the fake window is a known hwnd"
    );
    assert_eq!(
        state.window_state().host_geometry_request,
        Some((100, 200, 200, 100)),
        "the fake-window branch must forward the new geometry too"
    );
}

#[test]
fn test_set_window_placement_changed_rect_wakes_host_presenter() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let wake_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&wake_flag);
    state.present().wake = Some(Box::new(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    let hwnd = push_geometry_window(&mut state);
    let placement_ptr = 0x4000_u64;

    write_placement_struct(&mut engine, placement_ptr, 1, 20, 30, 220, 130);
    assert_eq!(
        dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr),
        1
    );
    assert!(
        wake_flag.load(std::sync::atomic::Ordering::SeqCst),
        "a changed rect must wake the host presenter"
    );

    // The same rect again: no new wake, and the pending request survives.
    wake_flag.store(false, std::sync::atomic::Ordering::SeqCst);
    write_placement_struct(&mut engine, placement_ptr, 1, 20, 30, 220, 130);
    assert_eq!(
        dispatch_set_window_placement(&mut engine, &mut state, hwnd, placement_ptr),
        1
    );
    assert!(
        !wake_flag.load(std::sync::atomic::Ordering::SeqCst),
        "an unchanged rect must not wake the host presenter"
    );
    assert_eq!(
        state.window_state().host_geometry_request,
        Some((20, 30, 200, 100)),
        "a pending request survives an unchanged follow-up call"
    );
}

// --- MessageBox (host bridge) ---

/// Register a test MessageBox bridge that records every `(caption, text,
/// mb_type)` call and answers with the canned Win32 id.
fn register_message_box_bridge(
    state: &mut WinApiState,
    canned: i32,
    captured: Arc<Mutex<Vec<(String, String, u32)>>>,
) {
    state.present().message_box_bridge = Some(Box::new(move |caption, text, mb_type| {
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((caption.to_owned(), text.to_owned(), mb_type));
        canned
    }));
}

/// Drive a MessageBox handler with a scripted host alert bridge (the bridge
/// must already be registered).
///
/// The real flow is two entries around the bridge: the handler's first entry
/// records [`PendingNativeMessageBox`] and returns
/// [`WinApiControlSignal::MessageBoxBridgeRequested`]; the runtime runs the
/// bridge WITHOUT the shared lock and records the chosen id; the engine's
/// re-execution of the fake API re-enters the handler, which returns the id
/// to the guest. This helper simulates exactly that (the runtime is not
/// involved in unit tests).
fn dispatch_message_box_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    api: fn(&mut HandlerContext<'_>) -> anyhow::Result<kernel32::WinApiHandlerResult>,
) -> anyhow::Result<kernel32::WinApiHandlerResult> {
    let first = api(&mut HandlerContext::new(engine, test_environment(), state))
        .expect_err("the first entry parks the guest for the host alert");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::MessageBoxBridgeRequested { request } = signal else {
        panic!("expected a message-box bridge request");
    };
    // What the runtime does between the two entries: take the bridge out,
    // run it (no shared lock), restore it, record the chosen id.
    let bridge = state
        .present()
        .message_box_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(&request.caption, &request.text, request.message_box_type);
    state.present().message_box_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_message_box
        .as_mut()
        .expect("pending session recorded")
        .pick = Some(picked);
    // Re-entry: the handler returns the chosen id to the guest.
    api(&mut HandlerContext::new(engine, test_environment(), state))
}

/// `MessageBoxW` decodes UTF-16 args, forwards them to the registered bridge
/// verbatim (mb_type included), and returns the bridge's id to the guest —
/// through the two-entry bridge flow (request → runtime runs the bridge →
/// re-entry resolves the pick).
#[test]
fn test_message_box_w_calls_registered_bridge_with_decoded_args() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 6, Arc::clone(&captured)); // IDYES
    write_guest_utf16(&mut engine, 0x6000, "Save changes?");
    write_guest_utf16(&mut engine, 0x7000, "notepad");
    // MB_YESNO | MB_ICONQUESTION = 0x4 | 0x20.
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x24, 0);

    let r = dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_w)
        .expect("MessageBoxW must dispatch");

    assert_eq!(r.return_value, 6, "the bridge's id must reach the guest");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "notepad", "caption decoded from UTF-16");
    assert_eq!(calls[0].1, "Save changes?", "text decoded from UTF-16");
    assert_eq!(
        calls[0].2, 0x24,
        "MB_* flag bits must pass through to the bridge verbatim"
    );
}

/// `MessageBoxA` decodes ANSI/UTF-8 args and reaches the bridge the same way
/// the wide variant does.
#[test]
fn test_message_box_a_decodes_ansi_and_calls_bridge() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 2, Arc::clone(&captured)); // IDCANCEL
    write_guest_ansi(&mut engine, 0x6000, "Unsaved changes");
    write_guest_ansi(&mut engine, 0x7000, "editor");
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x1, 0); // MB_OKCANCEL

    let r = dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_a)
        .expect("MessageBoxA must dispatch");

    assert_eq!(r.return_value, 2, "the bridge's id must reach the guest");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "editor", "caption decoded from ANSI");
    assert_eq!(calls[0].1, "Unsaved changes", "text decoded from ANSI");
    assert_eq!(calls[0].2, 0x1, "MB_OKCANCEL must pass through");
}

/// Every MB_* button/icon set reaches the bridge unchanged and the bridge's
/// canned result (Ok/Cancel/Yes/No → the Win32 id) is what the guest sees.
#[test]
fn test_message_box_flag_sets_pass_through_and_results_map_to_ids() {
    // (mb_type, canned bridge answer, expected guest return value).
    let cases: &[(u32, i32, u64)] = &[
        (0x00, 1, user32::IDOK),     // MB_OK → bridge answers Ok → IDOK
        (0x01, 2, user32::IDCANCEL), // MB_OKCANCEL → Cancel → IDCANCEL
        (0x04, 6, user32::IDYES),    // MB_YESNO → Yes → IDYES
        (0x04, 7, user32::IDNO),     // MB_YESNO → No → IDNO
        (0x03, 6, user32::IDYES),    // MB_YESNOCANCEL → Yes → IDYES
        (0x03, 2, user32::IDCANCEL), // MB_YESNOCANCEL → Cancel → IDCANCEL
        (0x10, 1, user32::IDOK),     // MB_ICONERROR
        (0x20, 1, user32::IDOK),     // MB_ICONQUESTION
        (0x30, 1, user32::IDOK),     // MB_ICONWARNING
        (0x40, 1, user32::IDOK),     // MB_ICONINFORMATION
    ];
    for &(mb_type, canned, expected) in cases {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
        register_message_box_bridge(&mut state, canned, Arc::clone(&captured));
        write_guest_utf16(&mut engine, 0x6000, "text");
        write_guest_utf16(&mut engine, 0x7000, "caption");
        write_regs(&mut engine, 0, 0x6000, 0x7000, u64::from(mb_type), 0);

        let r =
            dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_w)
                .expect("MessageBoxW must dispatch");

        assert_eq!(
            r.return_value, expected,
            "mb_type {mb_type:#06x} must surface the bridge's id"
        );
        let calls = captured.lock().expect("bridge capture lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2, mb_type,
            "mb_type {mb_type:#06x} must reach the bridge verbatim"
        );
    }
}

/// No bridge registered (headless runs, `trace`): the handler echoes to the
/// host console and auto-returns IDOK so the guest never hangs.
#[test]
fn test_message_box_without_bridge_falls_back_to_idok() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_guest_utf16(&mut engine, 0x6000, "text");
    write_guest_utf16(&mut engine, 0x7000, "caption");
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x4, 0);

    let r = user32::handle_message_box_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("MessageBoxW must dispatch without a bridge");

    assert_eq!(
        r.return_value,
        user32::IDOK,
        "headless fallback returns IDOK"
    );
}

// --- ShellAboutW (the same host message-box bridge) ---

/// `ShellAboutW` (shell32) routes through the SAME two-entry message-box
/// bridge flow as `MessageBoxA/W`: the first entry records the pending box
/// and returns [`WinApiControlSignal::MessageBoxBridgeRequested`]; the
/// runtime runs the bridge WITHOUT the shared lock; the re-entry resolves the
/// pick (IDOK for the OK-style About box) and returns TRUE.
#[test]
fn test_shell_about_w_routes_through_message_box_bridge() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 1, Arc::clone(&captured)); // IDOK
    write_guest_utf16(&mut engine, 0x6000, "Notepad Authors");
    write_guest_utf16(&mut engine, 0x7000, "Notepad");
    // ShellAboutW(hwnd, szAppName, szOtherStuff, hIcon).
    write_regs(&mut engine, 0, 0x7000, 0x6000, 0, 0);

    let r =
        dispatch_message_box_with_bridge(&mut engine, &mut state, shell32::handle_shell_about_w)
            .expect("ShellAboutW must dispatch");

    assert_eq!(r.return_value, 1, "ShellAboutW returns TRUE (IDOK)");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "Notepad", "szAppName is the caption");
    assert_eq!(calls[0].1, "Notepad Authors", "szOtherStuff is the text");
    assert_eq!(calls[0].2, 0, "ShellAboutW is an MB_OK alert");
}

/// No bridge registered (headless runs, `trace`): ShellAboutW echoes to the
/// host console and auto-returns TRUE so the guest never hangs.
#[test]
fn test_shell_about_w_without_bridge_falls_back_to_true() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_guest_utf16(&mut engine, 0x6000, "Notepad Authors");
    write_guest_utf16(&mut engine, 0x7000, "Notepad");
    write_regs(&mut engine, 0, 0x7000, 0x6000, 0, 0);

    let r = shell32::handle_shell_about_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("ShellAboutW must dispatch without a bridge");

    assert_eq!(r.return_value, 1, "headless fallback returns TRUE");
    assert!(
        state.window_state().pending_native_message_box.is_none(),
        "no pending box was recorded without a bridge"
    );
}

// --- PrintDlgW (native print-panel bridge) ---

/// Write a `PRINTDLG` (Win64) into guest memory at `pd_ptr` (the typed view
/// zero-fills the untouched fields — the layout lives in guest_layout).
fn write_print_dlg(
    engine: &mut IcedCpu,
    pd_ptr: u64,
    h_dev_mode: u64,
    h_dev_names: u64,
    flags: u32,
) {
    crate::guest_memory::with_typed_write::<crate::guest_layout::PrintDlgW, _, _>(
        engine,
        pd_ptr,
        |pd| {
            pd.l_struct_size = 120;
            pd.hwnd_owner = 0;
            pd.h_dev_mode = h_dev_mode;
            pd.h_dev_names = h_dev_names;
            pd.flags = flags;
            pd.n_from_page = 1;
            pd.n_to_page = 0xFFFF;
            pd.n_min_page = 1;
            pd.n_max_page = 0xFFFF;
            pd.n_copies = 1;
            Ok(())
        },
    )
    .expect("write PRINTDLG");
}

/// Seed the test heap's guest bump cursor — the control block at 0x2000 was
/// attached in `default_winapi_state` (the LocalAlloc test precedent),
/// otherwise `alloc_coherent` sees bump=0 < base and refuses to allocate.
fn seed_test_heap_bump(engine: &mut IcedCpu) {
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("write heap bump cursor");
}

/// Drive `PrintDlgW` with a scripted native print-panel bridge (Interactive
/// policy).
///
/// The real flow is two entries around the bridge: the handler's first entry
/// reads the `PRINTDLG`/DEVMODE, records [`PendingNativePrintDialog`] and
/// returns [`WinApiControlSignal::PrintDialogBridgeRequested`]; the runtime
/// runs the bridge WITHOUT the shared lock and records the pick; the engine's
/// re-execution of the fake API re-enters the handler, which allocates the
/// print DC and writes the pick back. This helper simulates exactly that (the
/// runtime is not involved in unit tests).
fn dispatch_print_dlg_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    bridge: crate::PrintDialogBridge,
) -> anyhow::Result<kernel32::WinApiHandlerResult> {
    seed_test_heap_bump(engine);
    state.window_state().print_dialog_policy = crate::PrintDialogPolicy::Interactive;
    state.window_state().print_dialog_bridge = Some(bridge);
    write_regs(engine, 0x5000, 0, 0, 0, 0);
    let first =
        comdlg32::handle_print_dlg_w(&mut HandlerContext::new(engine, test_environment(), state))
            .expect_err("the first entry parks the guest for the native print panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::PrintDialogBridgeRequested { request } = signal else {
        panic!("expected a print-dialog bridge request");
    };
    // What the runtime does between the two entries: take the bridge out, run
    // it (no shared lock), restore it, record the pick.
    let bridge = state
        .window_state()
        .print_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().print_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_print_dialog
        .as_mut()
        .expect("pending print dialog recorded")
        .pick = picked;
    // Re-entry: the handler writes the pick back.
    comdlg32::handle_print_dlg_w(&mut HandlerContext::new(engine, test_environment(), state))
}

/// The default `Cancel` policy (headless runs, `trace`): PrintDlgW returns
/// FALSE like a user canceling — no DC, no bridge, no write-back.
#[test]
fn test_print_dlg_cancel_policy_returns_false_without_machinery() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PrintDlgW must dispatch under Cancel");

    assert_eq!(r.return_value, 0, "Cancel → FALSE");
    assert!(
        state.gdi_state().dcs.is_empty(),
        "no print DC is allocated on cancel"
    );
    assert!(
        state.window_state().pending_native_print_dialog.is_none(),
        "no pending record on cancel"
    );
}

/// `Interactive` policy but NO bridge registered (headless/trace sessions):
/// the handler cancels so a guest never hangs on a panel nobody can click.
#[test]
fn test_print_dlg_interactive_without_bridge_falls_back_to_cancel() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().print_dialog_policy = crate::PrintDialogPolicy::Interactive;
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PrintDlgW must dispatch without a bridge");

    assert_eq!(r.return_value, 0, "no bridge → cancel");
    assert!(state.gdi_state().dcs.is_empty());
    assert!(state.window_state().pending_native_print_dialog.is_none());
}

/// `PD_RETURNDEFAULT` with NULL handles: the handler allocates fresh
/// DEVMODE/DEVNAMES blocks, writes the handles back into the `PRINTDLG`, and
/// returns FALSE (the documented query semantics — no panel).
#[test]
fn test_print_dlg_return_default_allocates_and_writes_default_blocks() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x400); // PD_RETURNDEFAULT
    seed_test_heap_bump(&mut engine);
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PD_RETURNDEFAULT must dispatch");

    assert_eq!(r.return_value, 0, "PD_RETURNDEFAULT returns FALSE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_ne!(pd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(pd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");
    assert_eq!(pd.h_dc, 0, "PD_RETURNDEFAULT never creates a DC");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, pd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the default DEVMODE");
    assert_eq!(dm.dm_size, 220, "a full DEVMODEW");
    assert_eq!(dm.dm_copies, 1);
    assert_eq!(dm.dm_orientation, 1, "portrait");

    // The DEVNAMES header: four WORD offsets, driver string at offset 8.
    let (driver_off, device_off, output_off) = {
        let mut bytes = [0_u8; 8];
        engine
            .mem_read(pd.h_dev_names, &mut bytes)
            .expect("read DEVNAMES header");
        (
            u16::from_le_bytes([bytes[0], bytes[1]]),
            u16::from_le_bytes([bytes[2], bytes[3]]),
            u16::from_le_bytes([bytes[4], bytes[5]]),
        )
    };
    assert_eq!(driver_off, 8);
    assert!(device_off > driver_off && output_off > device_off);
}

/// `PD_RETURNDEFAULT` with caller-provided blocks: the blocks are filled IN
/// PLACE (the handles stay the same) and FALSE is returned.
#[test]
fn test_print_dlg_return_default_fills_caller_blocks_in_place() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::{with_typed_read, with_typed_write};

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // The caller allocated DEVMODE + DEVNAMES blocks at 0x6000 / 0x6200.
    with_typed_write::<DevModeW, _, _>(&mut engine, 0x6000, |dm| {
        dm.dm_size = 220;
        Ok(())
    })
    .expect("write input DEVMODE block");
    engine
        .mem_write(0x6200, &[0_u8; 64])
        .expect("write input DEVNAMES block");
    write_print_dlg(&mut engine, 0x5000, 0x6000, 0x6200, 0x400); // PD_RETURNDEFAULT
    seed_test_heap_bump(&mut engine);
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PD_RETURNDEFAULT must dispatch");
    assert_eq!(r.return_value, 0);

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(
        pd.h_dev_mode, 0x6000,
        "the caller's DEVMODE block is reused"
    );
    assert_eq!(
        pd.h_dev_names, 0x6200,
        "the caller's DEVNAMES block is reused"
    );
    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, 0x6000, |dm| Ok(*dm))
        .expect("read the filled DEVMODE");
    assert_eq!(dm.dm_size, 220);
    assert_eq!(dm.dm_spec_version, 0x0401);
}

/// A bridge accept: PD_RETURNDC with no input DEVMODE → the pick's settings
/// are written back (hDC + nCopies + fresh DEVMODE/DEVNAMES blocks) and the
/// print job carries the paper/copies/print_info_id.
#[test]
fn test_print_dlg_bridge_accept_allocates_dc_and_writes_back() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::with_typed_read;
    use crate::handles::Hdc;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC

    let bridge: crate::PrintDialogBridge = Box::new(|request| {
        // No input DEVMODE → the panel seeds from the letter defaults.
        assert_eq!(request.paper_size_mm, (216, 279));
        assert_eq!(request.orientation, 1, "portrait default");
        assert_eq!(request.copies, 1);
        assert_eq!(request.color, 2, "color default");
        assert_ne!(request.print_info_id, 0);
        Some(crate::PrintDialogPick {
            paper_size_mm: (210, 297), // A4
            orientation: 1,
            copies: 2,
            color: 2,
            print_info_id: request.print_info_id,
        })
    });

    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1, "an accepted pick → TRUE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    let hdc = pd.h_dc;
    assert_ne!(hdc, 0, "PD_RETURNDC → a print DC is allocated");
    assert_eq!(pd.n_copies, 2, "the pick's copies reach the guest");
    assert_ne!(pd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(pd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, pd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the DEVMODE write-back");
    assert_eq!(dm.dm_paper_width, 2100, "A4 width in tenths of mm");
    assert_eq!(dm.dm_paper_length, 2970, "A4 length in tenths of mm");
    assert_eq!(dm.dm_paper_size, 9, "DMPAPER_A4");
    assert_eq!(dm.dm_copies, 2);
    assert_eq!(dm.dm_orientation, 1);

    // The DEVNAMES header is valid (driver string at offset 8).
    let mut bytes = [0_u8; 8];
    engine
        .mem_read(pd.h_dev_names, &mut bytes)
        .expect("read DEVNAMES header");
    assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 8);

    // The print job carries the pick (GetDeviceCaps / StartPage / EndDoc and
    // the P3 NSPrintInfo handoff all read from it).
    let job = state
        .gdi_state()
        .find_print_job(Hdc::from(hdc))
        .expect("the print job exists");
    assert_eq!(job.copies, 2);
    assert_eq!(job.paper_mm, (210, 297));
    assert_eq!(job.print_info_id, u32::try_from(1).unwrap_or(0));
}

/// A bridge cancel (the user pressed Cancel on the panel): PrintDlgW returns
/// FALSE and the PRINTDLG stays untouched (hDC 0, no new blocks).
#[test]
fn test_print_dlg_bridge_cancel_returns_false_without_write_back() {
    use crate::guest_layout::PrintDlgW;
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC

    let bridge: crate::PrintDialogBridge = Box::new(|_| None);
    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge cancel must dispatch");
    assert_eq!(r.return_value, 0, "a canceled pick → FALSE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(pd.h_dc, 0, "no DC on cancel");
    assert_eq!(pd.h_dev_mode, 0, "no DEVMODE allocation on cancel");
    assert_eq!(pd.h_dev_names, 0, "no DEVNAMES allocation on cancel");
    assert_eq!(pd.n_copies, 1, "nCopies untouched");
    assert!(state.gdi_state().dcs.is_empty());
}

/// A guest input DEVMODE seeds the panel (the bridge sees A4/landscape/3
/// copies from `dmPaperWidth`/`dmPaperLength`/`dmOrientation`/`dmCopies`) and
/// the accept reuses the SAME block for the write-back.
#[test]
fn test_print_dlg_bridge_seeds_from_guest_devmode_and_reuses_the_block() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use crate::handles::Hdc;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A guest DEVMODE: A4 landscape, 3 copies, monochrome.
    with_typed_write::<DevModeW, _, _>(&mut engine, 0x6000, |dm| {
        dm.dm_size = 220;
        dm.dm_paper_width = 2100;
        dm.dm_paper_length = 2970;
        dm.dm_paper_size = 9;
        dm.dm_orientation = 2;
        dm.dm_copies = 3;
        dm.dm_color = 1;
        Ok(())
    })
    .expect("write input DEVMODE");
    write_print_dlg(&mut engine, 0x5000, 0x6000, 0, 0x100); // PD_RETURNDC

    let seed = Arc::new(Mutex::new(None));
    let seed_capture = Arc::clone(&seed);
    let bridge: crate::PrintDialogBridge = Box::new(move |request| {
        *seed_capture.lock().expect("seed capture lock") = Some((
            request.paper_size_mm,
            request.orientation,
            request.copies,
            request.color,
        ));
        Some(crate::PrintDialogPick {
            paper_size_mm: request.paper_size_mm,
            orientation: request.orientation,
            copies: request.copies,
            color: request.color,
            print_info_id: request.print_info_id,
        })
    });

    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1);
    let (paper, orientation, copies, color) = seed
        .lock()
        .expect("seed capture lock")
        .expect("the bridge saw a request");
    assert_eq!(paper, (210, 297), "the guest DEVMODE seeds the paper");
    assert_eq!(orientation, 2, "the guest DEVMODE seeds the orientation");
    assert_eq!(copies, 3);
    assert_eq!(color, 1);

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(pd.h_dev_mode, 0x6000, "the guest's DEVMODE block is reused");
    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, 0x6000, |dm| Ok(*dm))
        .expect("read the rewritten DEVMODE");
    assert_eq!(dm.dm_copies, 3, "the round-trip keeps the copies");
    assert_eq!(dm.dm_paper_width, 2100);
    let job = state
        .gdi_state()
        .find_print_job(Hdc::from(pd.h_dc))
        .expect("the print job exists");
    assert_eq!(job.paper_mm, (210, 297));
    assert_eq!(job.copies, 3);
}

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
    let logfont_ptr = 0x5000_u64;
    engine
        .mem_write(logfont_ptr, &(-13_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_ptr + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_ptr + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_ptr + 28, b"MS Shell Dlg\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_ptr, 0, 0, 0, 0);
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

#[test]
fn test_edit_wm_vscroll_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "line1\nline2\nline3\nline4\nline5");

    // SB_LINEDOWN (1) scrolls the multiline EDIT (only 1 visible row in a
    // 20 px client) → the parent gets WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)).
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_VSCROLL.as_u32(),
        1, // SB_LINEDOWN
        0,
    );
    let error = result.expect_err("WM_VSCROLL must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "WM_VSCROLL must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), \
         got {signal:?}"
    );
}

#[test]
fn test_edit_wm_hscroll_delivers_en_hscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "line1\nline2");

    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_HSCROLL.as_u32(),
        0,
        0,
    );
    let error = result.expect_err("WM_HSCROLL must deliver EN_HSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0601_000C
        ),
        "WM_HSCROLL must deliver WM_COMMAND(MAKEWPARAM(12, EN_HSCROLL)), \
         got {signal:?}"
    );
}

#[test]
fn test_edit_pgup_pgdn_keydown_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // An 80 px tall control gives a 5-line page (the 16 px default line
    // height) — the same fixture as the PgUp/PgDn caret-movement test.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }

    // PgDn moves the caret a page (line 0 → 5): the parent gets the same
    // WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)) a real vertical scroll delivers,
    // so notepad re-reads the caret position into its status bar.
    let pgdn_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_NEXT,
        0,
    );
    let error = pgdn_result.expect_err("PgDn must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "PgDn must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // PgUp likewise (line 5 → 0).
    let pgup_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_PRIOR,
        0,
    );
    let error = pgup_result.expect_err("PgUp must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "PgUp must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // PgUp at the first line: the caret cannot move up a page, so the key is
    // a no-op — no EN_VSCROLL fires (the pragmatic no-move semantic).
    let noop_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_PRIOR,
        0,
    );
    let value = noop_result.expect("no-op PgUp at the top must not notify");
    assert_eq!(value, Some(0), "a no-op page key answers 0 silently");
    assert_eq!(
        control_ui(&state, edit).caret,
        0,
        "the caret stays at line 0"
    );
}

#[test]
fn test_edit_caret_blink_timer_toggles_caret_phase() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "line1\nline2\nline3");

    // Focus arms the internal blink timer and resets the caret to the on
    // phase (the caret bar shows solid until the first tick).
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
    let caret_timer = state
        .window_state()
        .timers
        .iter()
        .find(|t| t.window_handle == crate::handles::Hwnd::from(edit) && t.timer_id == 1)
        .expect("focus must arm the caret timer");
    assert_eq!(
        caret_timer.interval_ms, 530,
        "the blink half-period is the SPI_GETCARETTIMEOUT default"
    );
    assert!(
        control_ui(&state, edit).caret_on,
        "the caret starts in the on phase"
    );

    // Each WM_TIMER tick flips the phase, so the caret bar alternates
    // between drawn and hidden (the paint draws it only in the on phase).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1,
        0,
    )
    .expect("timer ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the first tick hides the caret"
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1,
        0,
    )
    .expect("timer ok")
    .expect("some result");
    assert!(
        control_ui(&state, edit).caret_on,
        "the second tick shows it again"
    );

    // A WM_TIMER with a different id is not the caret timer: not the edit's
    // business (falls through, no state change).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        99,
        0,
    )
    .expect("other timer ok");
    assert_eq!(r, None, "an unknown timer id must not touch the edit");

    // Losing focus disarms the timer and clears the focus flag (the paint
    // already skips the caret while unfocused).
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
    assert!(
        !state
            .window_state()
            .timers
            .iter()
            .any(|t| t.window_handle == crate::handles::Hwnd::from(edit)),
        "kill focus must disarm the caret timer"
    );
    assert!(
        !control_ui(&state, edit).focused,
        "kill focus clears the flag"
    );
}

/// The visual row where the last paint drew `hwnd`'s caret bar (the
/// `ControlState::Edit::last_caret_drawn_row` surface record).
fn edit_caret_drawn_row(state: &WinApiState, hwnd: u64) -> Option<usize> {
    match state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
    {
        Some(crate::user32::controls::ControlState::Edit {
            last_caret_drawn_row,
            ..
        }) => *last_caret_drawn_row,
        _ => None,
    }
}

/// The caret-blink tick must repaint BOTH the row where the last paint drew
/// the caret bar AND the caret's current row. A caret that moved since the
/// last paint leaves the old bar on the surface; a tick that repaints only
/// the current row would let that bar survive forever (the stuck/ghost
/// caret).
#[test]
fn test_edit_blink_tick_invalidates_last_drawn_and_current_caret_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: all three rows are visible (no auto-scroll on moves).
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Focus + paint: the caret (row 0) is drawn and its row recorded — the
    // surface now shows the bar on row 0.
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
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(0),
        "the paint must record the row where it drew the bar"
    );

    // Simulate the ghost interleaving: the caret moved to row 2 (the char
    // `a` of "gamma") after that paint and no repaint followed, so the bar
    // is still on the surface at row 0 while the caret lives on row 2.
    {
        let ws = state.window_state();
        let crate::user32::controls::ControlState::Edit { caret, .. } = ws
            .control_states
            .get_mut(&crate::handles::Hwnd::from(edit))
            .expect("edit state")
        else {
            panic!("edit state");
        };
        *caret = 12;
    }

    // The blink tick hides the bar and must repaint BOTH rows: the stale
    // row 0 (erase the old bar) and the caret's current row 2.
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
                if band.lo == 0 && band.hi >= 2
        ),
        "the tick must repaint the last-drawn row (0) and the current caret row (2), got {:?}",
        control_ui(&state, edit).invalid_rows
    );
}

/// A caret move while the blink phase is OFF must still repaint the row the
/// caret LEFT: the old bar is cleared once the phase returns (the span
/// invalidation covers the moved characters; the old caret's own row is
/// repainted unconditionally so a boundary move can never skip it).
#[test]
fn test_edit_caret_move_with_blink_off_covers_old_and_new_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: the caret move from row 0 to row 1 stays in view, so
    // no auto-scroll can widen the pending band to a full repaint.
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Focus + paint, then flip the blink phase OFF (one tick): the bar is
    // hidden, so the next repaint must still cover the rows the caret
    // travels — a repaint that skips the old row could leave a stale bar
    // from an earlier paint on the surface.
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
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the tick must hide the caret before the move"
    );

    // VK_DOWN moves the caret from row 0 to row 1. The EN_VSCROLL
    // notification reaches the parent: with no guest WndProc in this
    // fixture it resolves to a silent Ok (a guest-proc parent would
    // deliver a control signal instead).
    let moved = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DOWN,
        0,
    );
    assert!(moved.is_ok(), "VK_DOWN must be handled by the edit");
    assert_eq!(
        control_ui(&state, edit).caret,
        6,
        "the caret must land at the start of row 1"
    );
    assert!(
        matches!(
            control_ui(&state, edit).invalid_rows,
            crate::user32::controls::EditInvalidation::Band(band)
                if band.lo == 0 && band.hi >= 1
        ),
        "the move must repaint the old caret row (0) and the new one (1), got {:?}",
        control_ui(&state, edit).invalid_rows
    );
}

/// A mouse click focuses an EDIT without a WM_SETFOCUS: the blink phase must
/// reset to ON and the blink timer must re-arm, or a click on an edit whose
/// phase was left OFF (and whose timer a kill-focus disarmed) would leave
/// the caret invisible until the next key focus.
#[test]
fn test_edit_mouse_click_focus_resets_caret_blink_and_arms_timer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "alpha\nbeta\ngamma");

    // Focus, flip the phase OFF, then lose focus: the edit is left with the
    // caret hidden and the blink timer disarmed — the exact state a later
    // mouse click must repair.
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
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
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
    assert!(
        !control_ui(&state, edit).caret_on,
        "the edit must be left in the hidden blink phase"
    );
    assert!(
        !state
            .window_state()
            .timers
            .iter()
            .any(|t| { t.window_handle == crate::handles::Hwnd::from(edit) && t.timer_id == 1 }),
        "kill focus must have disarmed the blink timer"
    );

    // A mouse click on the edit (the EDIT WM_LBUTTONDOWN arm) focuses it
    // without a WM_SETFOCUS: the phase must come back ON and the timer must
    // re-arm with the 530 ms blink half-period.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_LBUTTONDOWN,
        0,
        u64::from((5_u32 << 16) | 5_u32), // lParam = (y << 16) | x
    )
    .expect("click ok")
    .expect("some result");
    assert!(
        control_ui(&state, edit).caret_on,
        "the click must reset the caret to the on phase"
    );
    assert!(
        control_ui(&state, edit).focused,
        "the click must focus the edit"
    );
    assert!(
        state.window_state().timers.iter().any(|t| {
            t.window_handle == crate::handles::Hwnd::from(edit)
                && t.timer_id == 1
                && t.interval_ms == 530
        }),
        "the click must re-arm the 530 ms caret timer"
    );
}

/// The paint records the row where it actually drew the caret bar — the
/// surface record the blink tick relies on. The record follows the caret
/// across paints and stays put when a paint skips the bar (blink phase off).
#[test]
fn test_edit_paint_records_the_row_where_the_caret_was_drawn() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: row 2 is on-screen, so the paint can actually draw the
    // bar there.
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Never painted: nothing recorded.
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        None,
        "a never-painted edit has no drawn bar row"
    );

    // Focus + paint: the bar lands on row 0 and is recorded.
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
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(0),
        "the first paint records the caret's row"
    );

    // The caret moves to row 2; the next paint records row 2.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        12,
        12,
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
    .expect("paint2 ok")
    .expect("some result");
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(2),
        "the record follows the caret to row 2"
    );

    // Blink off + paint: the bar is NOT drawn, so the record is untouched
    // (it still names the row the surface shows the bar on — the blink tick
    // erases it from there once the phase returns).
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
    assert!(
        !control_ui(&state, edit).caret_on,
        "the blink phase must be off for this paint"
    );
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(2),
        "a paint that skips the bar must leave the record untouched"
    );
}

#[test]
fn test_edit_typing_at_bottom_autoscrolls_caret_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // 5 visible rows in an 80 px client (the 16 px default line height):
    // typing on line 9 (char 18) must bring the caret into view instead of
    // leaving it off-screen below the last visible row.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        18,
        18,
    )
    .expect("setsel ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        0,
        "the fixture starts at the top"
    );

    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    )
    .expect_err("typing must deliver EN_CHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.message == 0x0111 && request.word_parameter == 0x0300_000C
        ),
        "typing must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        5,
        "typing past the last visible row scrolls the caret into view"
    );
    assert_eq!(
        control_ui(&state, edit).caret,
        19,
        "the typed char lands after the caret"
    );
}

#[test]
fn test_edit_arrow_keys_deliver_en_hscroll_and_en_vscroll() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");

    // A horizontal move (VK_RIGHT) delivers EN_HSCROLL — the status-bar
    // caret refresh for column changes.
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_RIGHT,
        0,
    )
    .expect_err("VK_RIGHT must deliver EN_HSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0601_000C
        ),
        "VK_RIGHT must deliver WM_COMMAND(MAKEWPARAM(12, EN_HSCROLL)), got {signal:?}"
    );

    // A vertical move (VK_DOWN) delivers EN_VSCROLL (caret 1 → line 1).
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DOWN,
        0,
    )
    .expect_err("VK_DOWN must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "VK_DOWN must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // Ctrl+End is a document-wide vertical jump → EN_VSCROLL (the line-aware
    // plain End would be EN_HSCROLL).
    state.window_state().keyboard_state.set(0x11, 0x80); // VK_CONTROL held
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_END,
        0,
    )
    .expect_err("Ctrl+End must deliver EN_VSCROLL");
    state.window_state().keyboard_state.set(0x11, 0);
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "Ctrl+End must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );
}

#[test]
fn test_edit_click_release_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // A completed click navigation (down then up) delivers EN_VSCROLL to the
    // parent — the status-bar caret refresh for click navigation.
    let (x, y) = (20_u16, 10_u16);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        x,
        y,
    );
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONUP.as_u32(),
        0,
        mouse_lparam(x, y),
    );
    let error = result.expect_err("a completed click must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "a completed click must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), \
         got {signal:?}"
    );
}

// --- Comdlg32 ---

/// Read a NUL-terminated UTF-16LE guest string at `addr` (up to `max_units`).
///
/// The `_raw` suffix distinguishes this test-side buffer reader from the
/// `pub(crate)` `read_utf16_lossy` guest-string helper of the same shape.
fn read_guest_utf16_raw(engine: &mut IcedCpu, addr: u64, max_units: usize) -> String {
    let byte_len = max_units
        .checked_mul(2)
        .expect("UTF-16 byte length overflow");
    let mut bytes = vec![0_u8; byte_len];
    engine
        .mem_read(addr, &mut bytes)
        .expect("read guest UTF-16 buffer");
    let mut units: Vec<u16> = Vec::new();
    for pair in bytes.chunks_exact(2) {
        let lo = pair.first().copied().expect("two-byte chunk lo");
        let hi = pair.get(1).copied().expect("two-byte chunk hi");
        let unit = u16::from_le_bytes([lo, hi]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

/// Read a NUL-terminated ANSI/UTF-8 guest string at `addr` (up to `max_bytes`).
///
/// The `_raw` suffix distinguishes this test-side buffer reader from the
/// `pub(crate)` `read_ansi_lossy` guest-string helper of the same shape.
fn read_guest_ansi_raw(engine: &mut IcedCpu, addr: u64, max_bytes: usize) -> String {
    let mut bytes = vec![0_u8; max_bytes];
    engine
        .mem_read(addr, &mut bytes)
        .expect("read guest ANSI buffer");
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(max_bytes);
    let head = bytes.get(..end).expect("ANSI head range");
    String::from_utf8_lossy(head).into_owned()
}

#[test]
fn test_get_file_title_w_basename() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\bar.txt");
    // Pre-fill so the handler's write is observable.
    engine
        .mem_write(title_addr, &[0xAA_u8; 128])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_eq!(r.return_value, 0, "GetFileTitleW must succeed");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "bar.txt",
        "basename after the last separator must be copied"
    );
}

#[test]
fn test_get_file_title_w_buffer_too_small() {
    // Truncated copy plus the MSDN negative return: abs = required size
    // including the terminating NUL ("bar.txt" is 7 chars → 8, returned -8).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\bar.txt");
    engine
        .mem_write(title_addr, &[0xAA_u8; 32])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 4, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(
        r.return_value, 0xFFFF_FFF8,
        "too-small buffer must return -(required size incl. NUL)"
    );
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 4),
        "bar",
        "buffer must hold a truncated NUL-terminated copy"
    );
}

#[test]
fn test_get_file_title_w_no_separators() {
    // No `\` or `/` in the path → the whole string is the title.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, "report.md");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 0, "GetFileTitleW must succeed");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "report.md",
        "a separator-free path must be copied whole"
    );
}

#[test]
fn test_get_file_title_w_trailing_separator_is_invalid() {
    // "C:\foo\" has no basename; GetFileTitle reports an invalid file name
    // (1) and the buffer still comes back NUL-terminated.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 1, "trailing separator must be invalid");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "",
        "buffer must be NUL-terminated"
    );
}

#[test]
fn test_get_file_title_w_empty_path_is_success() {
    // A genuinely empty path has no basename, but real GetFileTitle treats
    // it as success: 0 return with an empty NUL-terminated title (only a
    // trailing-separator path is an invalid file name).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, "");
    // Pre-fill so the handler's NUL write is observable.
    engine
        .mem_write(title_addr, &[0xAA_u8; 64])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 0, "empty path must succeed with 0");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "",
        "title buffer must be NUL-terminated"
    );
}

#[test]
fn test_get_file_title_a_basename_ansi() {
    // ANSI mirror: reads an A-string path and writes an A-string title.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_ansi(&mut engine, path_addr, r"C:\foo\bar.txt");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleA")
        .expect("GetFileTitleA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleA must dispatch");
    assert_eq!(r.return_value, 0, "GetFileTitleA must succeed");
    assert_eq!(
        read_guest_ansi_raw(&mut engine, title_addr, 64),
        "bar.txt",
        "ANSI basename must be copied"
    );
}

#[test]
fn test_choose_color_a_writes_color() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cc_ptr = 0x5000;
    engine
        .mem_map(cc_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map CHOOSECOLOR");
    write_regs(&mut engine, cc_ptr, 0, 0, 0, 0);
    assert_return_value!(
        comdlg32::handle_choose_color_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // rgbResult is at offset 0x10 in CHOOSECOLOR — should be RGB black (0).
    let mut rgb = [0_u8; 4];
    engine.mem_read(cc_ptr + 0x10, &mut rgb).ok();
    assert_eq!(u32::from_le_bytes(rgb), 0x00_00_00);
}

// --- Gdi32 ---

#[test]
fn test_text_out_a_returns_cch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 10, 20, 0x2000, 0x3000);
    // cchString at RSP+0x28 = 5.
    engine.mem_write(0x3028, &5_u32.to_le_bytes()).ok();
    assert_return_value!(
        gdi32::handle_text_out_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        5
    );
}

#[test]
fn test_bit_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_bit_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_stretch_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_stretch_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_pat_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_pat_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

// --- Advapi32 ---

#[test]
fn test_set_security_descriptor_dacl_null_fails() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_return_value!(
        advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_set_security_descriptor_dacl_valid_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1000, 1, 0x2000, 1, 0);
    assert_return_value!(
        advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
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
    let prev_ptr = 0x4000;
    write_regs(&mut engine, 0x03, prev_ptr, 0, 0, STACK_TOP);
    let r = kernel32::handle_set_thread_error_mode(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetThreadErrorMode");
    assert_eq!(r.return_value, 1); // TRUE
    assert_eq!(state.process.error_mode, 3);
    let mut buf = [0_u8; 4];
    engine.mem_read(prev_ptr, &mut buf).ok();
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
    // "Not found" is only reachable with a bottle: without one the
    // bottle-enforcement policy stops the run on the first file op instead
    // (BottleMissingError). The root need not exist — the probe path does
    // not exist under any root, which is exactly the case under test.
    state.file_io.volumes.bottle_root = Some(std::path::PathBuf::from("/tmp/wie-bottle"));
    let path_ptr = 0x3000;
    engine
        .mem_write(
            path_ptr,
            &"C:\\nonexistent"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .ok();
    engine.mem_write(path_ptr.wrapping_add(26), &[0, 0]).ok();
    write_regs(
        &mut engine,
        path_ptr,
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

// ── Shell32 ───────────────────────────────────────────────────────

#[test]
fn test_command_line_to_argv_w() {
    use crate::guest_string::write_utf16_c_string;
    let mut engine = test_engine();
    let mut state = winapi_state_default();
    let cmd_ptr = 0x3000;
    let num_args_ptr = 0x4000;
    // Write "hello" as the command line.
    write_utf16_c_string(&mut engine, cmd_ptr, 10, "hello").ok();
    engine.mem_write(num_args_ptr, &[0_u8; 4]).ok();
    // Call handler directly.
    write_regs(&mut engine, cmd_ptr, num_args_ptr, 0, 0, STACK_TOP);
    let result = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        shell32::dispatch_shell32(&mut ctx, "CommandLineToArgvW")
    }
    .expect("dispatch failed")
    .expect("handler not found");
    assert!(result.return_value != 0, "return_value is 0");
    let mut argc_buf = [0_u8; 4];
    engine.mem_read(num_args_ptr, &mut argc_buf).ok();
    assert_eq!(u32::from_le_bytes(argc_buf), 1);
}

#[test]
fn test_drag_accept_files_sets_and_clears_accepts_drops_flag() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = 0x6610_0001_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    // DragAcceptFiles(hwnd, TRUE) — must set the drop-accept flag.
    write_regs(&mut engine, hwnd, 1, 0, 0, 0);
    let id = crate::resolve_winapi_id("shell32.dll", "DragAcceptFiles")
        .expect("DragAcceptFiles must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("DragAcceptFiles must dispatch");
    assert_eq!(
        r.return_value, 1,
        "DragAcceptFiles returns void; non-zero mirrors the void-handler convention"
    );
    let ws = state.window_state();
    let window = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window must exist");
    assert!(
        window.flags.contains(WindowFlags::DROP_ACCEPTED),
        "TRUE must set the drop-accept flag"
    );
    // DragAcceptFiles(hwnd, FALSE) — must clear the flag.
    write_regs(&mut engine, hwnd, 0, 0, 0, 0);
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("DragAcceptFiles must dispatch again");
    assert_eq!(r.return_value, 1, "FALSE call must still return non-zero");
    let ws = state.window_state();
    let window = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window must exist");
    assert!(
        !window.flags.contains(WindowFlags::DROP_ACCEPTED),
        "FALSE must clear the drop-accept flag"
    );
}

// ── OLEAUT32 ──────────────────────────────────────────────────────

#[test]
fn test_var_add() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let presult = 0x3000;
    let plhs = 0x4000;
    let prhs = 0x5000;
    // lhs = VT_I4, value = 10
    engine.mem_write(plhs, &(3_u16).to_le_bytes()).ok(); // VT_I4
    engine
        .mem_write(plhs.wrapping_add(8), &10_u64.to_le_bytes())
        .ok();
    // rhs = VT_I4, value = 20
    engine.mem_write(prhs, &(3_u16).to_le_bytes()).ok();
    engine
        .mem_write(prhs.wrapping_add(8), &20_u64.to_le_bytes())
        .ok();
    write_regs(&mut engine, presult, plhs, prhs, 0, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        oleaut32::dispatch_oleaut32(&mut ctx, "VarAdd")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // S_OK
    let mut result_vt = [0_u8; 2];
    engine.mem_read(presult, &mut result_vt).ok();
    assert_eq!(u16::from_le_bytes(result_vt), 3); // VT_I4
    let mut result_val = [0_u8; 8];
    engine
        .mem_read(presult.wrapping_add(8), &mut result_val)
        .ok();
    assert_eq!(i64::from_le_bytes(result_val), 30);
}

#[test]
fn test_var_bstr_from_i4() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let presult = 0x3000;
    // VarBstrFromI4(42, 0, 0, &result)
    write_regs(&mut engine, presult, 42, 0, 0, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        oleaut32::dispatch_oleaut32(&mut ctx, "VarBstrFromI4")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // S_OK
    let mut vt = [0_u8; 2];
    engine.mem_read(presult, &mut vt).ok();
    assert_eq!(u16::from_le_bytes(vt), 8); // VT_BSTR
}

// ── ADVAPI32 ──────────────────────────────────────────────────────

#[test]
fn test_reg_enum_key_ex() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Create a registry key with parent 0x7000_0001 (HKEY_CURRENT_USER)
    let parent = 0x7000_0001;
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent,
        subkey: "Software\\test".into(),
    });
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x101,
        parent: 0x100,
        subkey: "Nested".into(),
    });
    let name_buf = 0x4000;
    let name_len_ptr = 0x5000;
    let name_len: u32 = 32;
    engine.mem_write(name_len_ptr, &name_len.to_le_bytes()).ok();
    // RegEnumKeyExW(hKey=0x100, dwIndex=0, lpName=name_buf, lpcchName=name_len_ptr, ...)
    write_regs(&mut engine, 0x100, 0, name_buf, name_len_ptr, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // ERROR_SUCCESS
    let mut len_out = [0_u8; 4];
    engine.mem_read(name_len_ptr, &mut len_out).ok();
    assert_eq!(u32::from_le_bytes(len_out), 6); // "Nested" length
}

#[test]
fn test_reg_enum_value_returns_no_more() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0x4000, 0x5000, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumValueW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 259); // ERROR_NO_MORE_ITEMS
}

/// The real `HKEY_CURRENT_USER` constant the guest bakes into its call site.
const HKEY_CURRENT_USER: u64 = 0x8000_0001;

/// Run `RegOpenKeyA/W` through the full dispatch path (names.rs → dense id → arm).
fn reg_open_key(library: &str, name: &str, state: &mut WinApiState, engine: &mut IcedCpu) -> u64 {
    let id = crate::resolve_winapi_id(library, name).expect("must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("RegOpenKey must dispatch");
    r.return_value
}

/// Read an 8-byte guest value (the `phkResult` handle output).
fn read_guest_handle(engine: &mut IcedCpu, addr: u64) -> u64 {
    let mut buf = [0_u8; 8];
    engine.mem_read(addr, &mut buf).expect("read guest handle");
    u64::from_le_bytes(buf)
}

#[test]
fn test_reg_open_key_w_opens_existing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_utf16(&mut engine, subkey_ptr, "Software\\Microsoft\\Notepad");
    // Sentinel: a failed open must not leave stale data behind.
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyW(hKey=HKCU, lpSubKey=subkey_ptr, phkResult=phk_ptr)
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, phk_ptr, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyW", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0x100);
}

#[test]
fn test_reg_open_key_w_missing_returns_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_utf16(&mut engine, subkey_ptr, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, phk_ptr, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyW", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0);
    // Open-only: the missing key must not be materialized by the legacy pair.
    assert!(state.process.registry_keys.is_empty());
}

#[test]
fn test_reg_open_key_a_matches_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_ansi(&mut engine, subkey_ptr, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyA(hKey=HKCU, lpSubKey=subkey_ptr, phkResult=phk_ptr)
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, phk_ptr, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0x100);
    // ANSI missing path mirrors the W variant.
    let missing_ptr = 0x5000;
    let missing_phk = 0x3100;
    write_guest_ansi(&mut engine, missing_ptr, "Software\\Missing");
    engine
        .mem_write(missing_phk, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(
        &mut engine,
        HKEY_CURRENT_USER,
        missing_ptr,
        missing_phk,
        0,
        0,
    );
    let status = reg_open_key("advapi32.dll", "RegOpenKeyA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, missing_phk), 0);
}

/// `RegOpenKeyExA` is open-only: a missing key must report
/// `ERROR_FILE_NOT_FOUND` and write 0 to `*phkResult` WITHOUT creating the
/// key — only `RegCreateKeyEx*` may materialize a key.
#[test]
fn test_reg_open_key_ex_a_missing_returns_file_not_found_and_does_not_create() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_ansi(&mut engine, subkey_ptr, "Software\\Missing\\Key");
    // RegOpenKeyExA passes phkResult in the 5th stack slot: [rsp+0x30].
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_ptr))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyExA(hKey=HKCU, lpSubKey=subkey_ptr, ulOptions=0, samDesired=0, phkResult=[rsp+0x30])
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0);
    // The key must not be materialized: a second open on the same path fails
    // identically (and again zeroes the output handle).
    assert!(state.process.registry_keys.is_empty());
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // Re-arm the registers: the first dispatch clobbers them on return.
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0);
    assert!(state.process.registry_keys.is_empty());
}

/// The W variant (soft-dispatch path) must match the A variant's open-only
/// semantics: ERROR_FILE_NOT_FOUND, *phkResult = 0, no creation.
#[test]
fn test_reg_open_key_ex_w_missing_returns_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_utf16(&mut engine, subkey_ptr, "Software\\Missing\\Key");
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_ptr))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegOpenKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0);
    assert!(state.process.registry_keys.is_empty());
}

/// `RegOpenKeyExA` on an EXISTING key still returns the stored handle.
#[test]
fn test_reg_open_key_ex_a_opens_existing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    write_guest_ansi(&mut engine, subkey_ptr, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_ptr))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_ptr, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0x100);
}

/// `RegCreateKeyExA` is the ONLY entry point allowed to create: the same
/// missing path that failed to open is materialized here, and a subsequent
/// open then succeeds with the created handle.
#[test]
fn test_reg_create_key_ex_a_creates_missing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Pre-existing key so the created handle is nonzero and distinguishable.
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Existing".into(),
    });
    state.process.next_registry_key_handle = crate::RegistryKeyHandle::from(0x101);
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    let disposition_ptr = 0x3100;
    write_guest_ansi(&mut engine, subkey_ptr, "Software\\Missing\\Key");
    // RegCreateKeyExA passes phkResult at [rsp+0x40] and lpdwDisposition at [rsp+0x48].
    engine
        .mem_write(STACK_TOP + 0x40, &u64::to_le_bytes(phk_ptr))
        .expect("write phkResult arg");
    engine
        .mem_write(STACK_TOP + 0x48, &u64::to_le_bytes(disposition_ptr))
        .expect("write lpdwDisposition arg");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegCreateKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0x101);
    let mut disp = [0_u8; 4];
    engine
        .mem_read(disposition_ptr, &mut disp)
        .expect("read disposition");
    assert_eq!(u32::from_le_bytes(disp), 1); // REG_CREATED_NEW_KEY
    // The previously-missing path now opens with the created handle.
    let open_phk = 0x3200;
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(open_phk))
        .expect("write phkResult arg");
    engine
        .mem_write(open_phk, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, open_phk), 0x101);
}

/// W-variant parity for the create path (soft dispatch, what notepad uses).
#[test]
fn test_reg_create_key_ex_w_creates_missing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Existing".into(),
    });
    state.process.next_registry_key_handle = crate::RegistryKeyHandle::from(0x101);
    let subkey_ptr = 0x5000;
    let phk_ptr = 0x3000;
    let disposition_ptr = 0x3100;
    write_guest_utf16(&mut engine, subkey_ptr, "Software\\Missing\\Key");
    engine
        .mem_write(STACK_TOP + 0x40, &u64::to_le_bytes(phk_ptr))
        .expect("write phkResult arg");
    engine
        .mem_write(STACK_TOP + 0x48, &u64::to_le_bytes(disposition_ptr))
        .expect("write lpdwDisposition arg");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_ptr, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegCreateKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_ptr), 0x101);
    let mut disp = [0_u8; 4];
    engine
        .mem_read(disposition_ptr, &mut disp)
        .expect("read disposition");
    assert_eq!(u32::from_le_bytes(disp), 1); // REG_CREATED_NEW_KEY
}

// ── USER32 ────────────────────────────────────────────────────────

#[test]
fn test_get_menu_returns_zero_for_unknown_window() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_get_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_menu_returns_menu_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = 0x100;
    let hmenu = 0x200;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        menu_handle: hmenu,
        ..Default::default()
    });
    write_regs(&mut engine, hwnd, 0, 0, 0, STACK_TOP);
    let r = user32::handle_get_menu(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMenu");
    assert_eq!(r.return_value, hmenu);
}

// ── USER32 menu tree (P1): MenuRecord / MenuEntry / menu_dirty ─────

/// Invoke `CreateMenu` and return the allocated handle.
fn create_menu(state: &mut WinApiState) -> u64 {
    let mut engine = test_engine();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    let r = user32::handle_create_menu(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        state,
    ))
    .expect("CreateMenu");
    r.return_value
}

/// Invoke `AppendMenuA(menu, flags, item_id, text_ptr)`, writing `text`
/// into guest memory first (empty text uses a null pointer).
fn append_menu_a(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    menu: u64,
    flags: u32,
    item_id: u32,
    text: &str,
) {
    let text_ptr = if text.is_empty() {
        0
    } else {
        engine
            .mem_write(0x2000, text.as_bytes())
            .expect("write menu text");
        0x2000
    };
    write_regs(
        engine,
        menu,
        u64::from(flags),
        u64::from(item_id),
        text_ptr,
        0,
    );
    assert_return_value!(
        user32::handle_append_menu_a(&mut HandlerContext::new(engine, test_environment(), state,)),
        1
    );
}

/// A menu with `Item(100, "Exit")`, a `Separator`, and `Item(200, "About")`.
fn push_two_item_menu(state: &mut WinApiState) -> u64 {
    let menu = create_menu(state);
    let mut engine = test_engine();
    append_menu_a(&mut engine, state, menu, 0x0000, 100, "Exit");
    append_menu_a(&mut engine, state, menu, crate::user32::MF_SEPARATOR, 0, "");
    append_menu_a(&mut engine, state, menu, 0x0000, 200, "About");
    menu
}

#[test]
fn test_append_menu_builds_native_tree() {
    let mut state = default_winapi_state();
    let menu = create_menu(&mut state);
    assert!(
        !state.window_state().menu_dirty,
        "CreateMenu alone must not dirty the tree (empty menu changes nothing)"
    );
    let popup = create_menu(&mut state);
    let mut engine = test_engine();
    append_menu_a(&mut engine, &mut state, menu, 0x0000, 100, "Exit");
    append_menu_a(
        &mut engine,
        &mut state,
        menu,
        crate::user32::MF_SEPARATOR,
        0,
        "",
    );
    append_menu_a(
        &mut engine,
        &mut state,
        menu,
        crate::user32::MF_POPUP,
        u32::try_from(popup).expect("popup handle"),
        "File",
    );

    assert!(state.window_state().menu_dirty, "AppendMenu must dirty");
    let record = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == crate::handles::Hmenu::from(menu))
        .expect("menu record exists");
    assert_eq!(record.items.len(), 3);
    assert!(
        matches!(
            record.items.first(),
            Some(crate::user32::menu::MenuEntry::Item { id, text, enabled: true, checked: false })
                if *id == 100 && text == "Exit"
        ),
        "first entry must be the Exit item"
    );
    assert!(
        matches!(
            record.items.get(1),
            Some(crate::user32::menu::MenuEntry::Separator)
        ),
        "second entry must be a separator"
    );
    assert!(
        matches!(
            record.items.get(2),
            Some(crate::user32::menu::MenuEntry::Popup { text, submenu })
                if text == "File" && submenu.as_u64() == popup
        ),
        "third entry must be the File popup linking the submenu handle"
    );
}

#[test]
fn test_get_menu_state_by_command_and_position() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    // By command: an enabled, unchecked item reports flag 0.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // By position: index 1 is the separator.
    write_regs(&mut engine, menu, 1, 0x0400, 0, 0); // MF_BYPOSITION
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(crate::user32::MF_SEPARATOR)
    );

    // Unknown command id returns -1.
    write_regs(&mut engine, menu, 999, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
}

#[test]
fn test_enable_menu_item_mutates_item_state() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    // First enable returns the previous state (0 = enabled) and grays it.
    write_regs(&mut engine, menu, 100, 0x0001, 0, 0); // MF_GRAYED
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert!(
        matches!(
            state
                .window_state()
                .menus
                .iter()
                .find(|m| m.handle == crate::handles::Hmenu::from(menu))
                .expect("record")
                .items
                .first(),
            Some(crate::user32::menu::MenuEntry::Item { enabled: false, .. })
        ),
        "MF_GRAYED must disable the item in the tree"
    );

    // GetMenuState now reports MF_GRAYED.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0001
    );

    // Re-enabling returns the previous MF_GRAYED state.
    write_regs(&mut engine, menu, 100, 0x0000, 0, 0); // MF_ENABLED
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0001
    );

    // A missing item returns -1 and does not mutate.
    let dirty_before = state.window_state().menu_dirty;
    write_regs(&mut engine, menu, 999, 0x0001, 0, 0);
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
    assert_eq!(state.window_state().menu_dirty, dirty_before);
}

#[test]
fn test_check_menu_item_mutates_item_state() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    write_regs(&mut engine, menu, 200, 0x0008, 0, 0); // MF_CHECKED
    assert_return_value!(
        user32::handle_check_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, menu, 200, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0008
    );

    // Unchecking returns the previous MF_CHECKED state.
    write_regs(&mut engine, menu, 200, 0x0000, 0, 0);
    assert_return_value!(
        user32::handle_check_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0008
    );
    write_regs(&mut engine, menu, 200, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
}

#[test]
fn test_menu_dirty_flag_semantics() {
    let mut state = default_winapi_state();
    assert!(!state.window_state().menu_dirty);

    // SetMenu to a new menu handle dirties (the window's menu changed).
    let menu = create_menu(&mut state);
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(0x100),
        ..Default::default()
    });
    state.window_state().menu_dirty = false;
    let mut engine = test_engine();
    write_regs(&mut engine, 0x100, menu, 0, 0, 0);
    assert_return_value!(
        user32::handle_set_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(state.window_state().menu_dirty, "SetMenu must dirty");

    // DestroyMenu removes the record and dirties.
    state.window_state().menu_dirty = false;
    write_regs(&mut engine, menu, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_destroy_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(state.window_state().menu_dirty, "DestroyMenu must dirty");
    assert!(
        state
            .window_state()
            .menus
            .iter()
            .all(|m| m.handle != crate::handles::Hmenu::from(menu)),
        "DestroyMenu must drop the record"
    );

    // GetMenuState on the destroyed menu now returns -1.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
}

// ── LoadMenuA/W from RT_MENU resources ─────────────────────────────

/// Seed the main-module menu templates with notepad-like entries: menu id
/// 0x201 (the resource notepad's window class names via `lpszMenuName`), a
/// File popup with Exit/separator/About and an Edit popup with Paste.
fn push_menu_templates(state: &mut WinApiState) {
    use wie_pe::resources::{MenuItemTemplate, MenuTemplate};
    state.process.main_module_menus.push(MenuTemplate {
        id: 0x201,
        lang: 0x0409,
        items: vec![
            MenuItemTemplate {
                flags: crate::user32::MF_POPUP,
                id: 0,
                text: Some("File".to_owned()),
                sub: vec![
                    MenuItemTemplate {
                        flags: 0x00,
                        id: 0x0100,
                        text: Some("Exit".to_owned()),
                        sub: Vec::new(),
                    },
                    MenuItemTemplate {
                        // The windres separator: option 0 + id 0 + empty
                        // text, NOT the MF_SEPARATOR bit — the parser keeps
                        // it as flags 0 / text None (real notepad's bytes).
                        flags: 0x00,
                        id: 0,
                        text: None,
                        sub: Vec::new(),
                    },
                    MenuItemTemplate {
                        flags: 0x00,
                        id: 0x0101,
                        text: Some("About".to_owned()),
                        sub: Vec::new(),
                    },
                ],
            },
            MenuItemTemplate {
                flags: crate::user32::MF_POPUP,
                id: 0,
                text: Some("Edit".to_owned()),
                sub: vec![MenuItemTemplate {
                    flags: 0x00,
                    id: 0x0110,
                    text: Some("Paste".to_owned()),
                    sub: Vec::new(),
                }],
            },
        ],
    });
}

#[test]
fn test_load_menu_w_returns_handle_for_known_resource() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // MAKEINTRESOURCEW(0x201): hinst = image base, low word = menu id.
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_ne!(first, 0, "known menu id must return a nonzero HMENU");

    // Repeated loads of the same id cache to the same handle.
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let second = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(first, second, "the same menu id must return the same HMENU");

    // The bar records the two top-level popups, each with a submenu.
    let record = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == crate::handles::Hmenu::from(first))
        .expect("menu record exists");
    assert_eq!(record.items.len(), 2, "File + Edit top-level popups");
    assert!(
        matches!(
            record.items.first(),
            Some(crate::user32::menu::MenuEntry::Popup { text, submenu })
                if text == "File" && submenu.as_u64() != 0
        ),
        "first top-level entry must be the File popup with a submenu"
    );

    // RNotepad's real MAIN_MENU separators survive the template→tree
    // conversion as `Separator` entries even though windres emitted them
    // with a zero option word (no MF_SEPARATOR bit).
    let file_submenu = match record.items.first() {
        Some(crate::user32::menu::MenuEntry::Popup { submenu, .. }) => *submenu,
        _ => panic!("first entry must be the File popup"),
    };
    let file = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == file_submenu)
        .expect("File submenu record");
    assert_eq!(file.items.len(), 3, "Exit / separator / About");
    assert!(
        matches!(
            file.items.get(1),
            Some(crate::user32::menu::MenuEntry::Separator)
        ),
        "a windres separator (flags 0, empty text) must map to MenuEntry::Separator"
    );

    // GetMenuState answers by command for an item nested in the File popup.
    write_regs(&mut engine, first, 0x0100, 0, 0, 0); // MF_BYCOMMAND
    let flags = dispatch_user32(&mut engine, &mut state, "GetMenuState");
    assert_eq!(flags, 0, "Exit is enabled + unchecked → flag 0");
}

#[test]
fn test_load_menu_w_unknown_id_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // Menu id 0x999 is not in the parsed set.
    write_regs(&mut engine, image_base, 0x999, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(handle, 0, "unknown menu id must return NULL");
}

#[test]
fn test_load_menu_w_named_menu_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // A string-named menu (high word set) has no parsed name→template
    // mapping yet; WIE returns NULL just like an unknown id.
    write_regs(&mut engine, image_base, 0x0000_0000_4000_0000, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(handle, 0, "string-named menus must return NULL");
}

#[test]
fn test_load_menu_a_mirrors_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuA");
    assert_ne!(handle, 0, "known menu id must return a nonzero HMENU via A");
    write_regs(&mut engine, handle, 0x0110, 0, 0, 0); // Edit → Paste
    let flags = dispatch_user32(&mut engine, &mut state, "GetMenuState");
    assert_eq!(flags, 0, "Paste is enabled + unchecked → flag 0");
}

#[test]
fn test_create_window_inherits_class_menu_name() {
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // A class whose lpszMenuName is MAKEINTRESOURCE(0x201), registered by the
    // main module (notepad's pattern: RegisterClassExW + CreateWindowExW with
    // hMenu = NULL).
    let atom = crate::user32::register_window_class(
        &mut state,
        WindowClassRecord {
            atom: 0,
            class_name: "NotepadClass".to_owned(),
            window_proc: 0x7000_0000,
            style: 0,
            instance_handle: image_base,
            icon_handle: 0,
            cursor_handle: 0,
            background_brush: 0,
            small_icon_handle: 0,
            unicode: true,
            menu_name: 0x201,
        },
    )
    .expect("register class");
    assert_ne!(atom, 0);

    let (hwnd, _, _) = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("NotepadClass".to_owned()),
            title: "Untitled - Notepad".to_owned(),
            style: 0,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0, // NULL → the class menu must be used
            instance_handle: image_base,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        },
        true,
    )
    .expect("create window");
    assert_ne!(hwnd, 0);

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record exists");
    assert_ne!(
        window.menu_handle, 0,
        "a window without an explicit hMenu must inherit the class menu"
    );
    assert!(
        state.window_state().menu_dirty,
        "attaching a menu must dirty"
    );
}

#[test]
fn test_register_class_ex_w_class_menu_end_to_end() {
    // The full guest path: a WNDCLASSEXW struct in guest memory whose
    // lpszMenuName is MAKEINTRESOURCEW(0x201) is read by RegisterClassExW,
    // and a CreateWindowExW with hMenu = NULL inherits the class menu.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // Prime the guest heap control block so CreateWindowExW's CREATESTRUCT
    // allocation (a coherent LocalAlloc-style block) succeeds.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("guest heap bump cursor");

    // WNDCLASSEXW at 0x4000 (misc.rs layout: cbSize@0, style@4, lpfnWndProc@8,
    // cbClsExtra@0x10, cbWndExtra@0x14, hInstance@0x18, hIcon@0x20,
    // hCursor@0x28, hbrBackground@0x30, lpszMenuName@0x38, lpszClassName@0x40,
    // hIconSm@0x48).
    let wc = 0x4000_u64;
    engine
        .mem_write(wc, &80_u32.to_le_bytes())
        .expect("WNDCLASSEXW.cbSize");
    engine
        .mem_write(wc + 4, &0_u32.to_le_bytes())
        .expect("WNDCLASSEXW.style");
    engine
        .mem_write(wc + 8, &0x7000_0000_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpfnWndProc");
    engine
        .mem_write(wc + 0x10, &0_i32.to_le_bytes())
        .expect("WNDCLASSEXW.cbClsExtra");
    engine
        .mem_write(wc + 0x14, &0_i32.to_le_bytes())
        .expect("WNDCLASSEXW.cbWndExtra");
    engine
        .mem_write(wc + 0x18, &image_base.to_le_bytes())
        .expect("WNDCLASSEXW.hInstance");
    engine
        .mem_write(wc + 0x20, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hIcon");
    engine
        .mem_write(wc + 0x28, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hCursor");
    engine
        .mem_write(wc + 0x30, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hbrBackground");
    engine
        .mem_write(wc + 0x38, &0x201_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpszMenuName");
    write_guest_utf16(&mut engine, 0x5000, "NotepadClass");
    engine
        .mem_write(wc + 0x40, &0x5000_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpszClassName");
    engine
        .mem_write(wc + 0x48, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hIconSm");

    // RegisterClassExW through the full dispatch path (names.rs → dense id).
    write_regs(&mut engine, wc, 0, 0, 0, 0);
    let atom = dispatch_user32(&mut engine, &mut state, "RegisterClassExW");
    assert_ne!(atom, 0, "the class must register");

    // CreateWindowExW(0, <class atom>, L"Untitled", 0, ..., hMenu = NULL).
    // The class has a guest WndProc, so the handler returns the WM_CREATE
    // bridge; the hwnd arrives in the callback's OuterReturn.
    write_guest_utf16(&mut engine, 0x6000, "Untitled - Notepad");
    write_regs(&mut engine, 0, atom, 0x6000, 0, 0x3000);
    for (slot, bytes) in [
        (0x3028_u64, 0_i32.to_le_bytes()), // X
        (0x3030, 0_i32.to_le_bytes()),     // Y
        (0x3038, 640_i32.to_le_bytes()),   // nWidth
        (0x3040, 480_i32.to_le_bytes()),   // nHeight
    ] {
        engine
            .mem_write(slot, &bytes)
            .expect("CreateWindowExW int arg");
    }
    engine
        .mem_write(0x3048, &0_u64.to_le_bytes())
        .expect("hWndParent");
    engine
        .mem_write(0x3050, &0_u64.to_le_bytes())
        .expect("hMenu NULL");
    engine
        .mem_write(0x3058, &image_base.to_le_bytes())
        .expect("hInstance");
    engine
        .mem_write(0x3060, &0_u64.to_le_bytes())
        .expect("lpParam");
    let id = crate::resolve_winapi_id("user32.dll", "CreateWindowExW")
        .expect("CreateWindowExW must resolve to a WinApiId");
    let result = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, default_env(), &mut state),
        id,
    );
    let error = result.expect_err("CreateWindowExW with a guest WndProc requests WM_CREATE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    let WinApiControlSignal::GuestCallbackRequested { request } = signal else {
        panic!("expected the WM_CREATE callback, got {signal:?}");
    };
    let OuterReturn::CreateWindow(hwnd) = request.outer_return else {
        panic!("expected OuterReturn::CreateWindow");
    };
    assert_ne!(hwnd, 0);

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record exists");
    assert_ne!(
        window.menu_handle, 0,
        "a window without an explicit hMenu must inherit the class menu"
    );
    assert!(
        state.window_state().menu_dirty,
        "attaching a menu must dirty"
    );
}

// ── GDI32 ─────────────────────────────────────────────────────────

#[test]
fn test_get_stock_object_white_brush() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    let r = gdi32::handle_get_stock_object(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetStockObject(WHITE_BRUSH)");
    assert!(r.return_value != 0);
}

#[test]
fn test_get_stock_object_unknown_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xFF, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_get_stock_object(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

// ── SEH hardware fault dispatch ──────────────────────────────────

#[test]
fn test_dispatch_hardware_fault_unhandled_returns_error() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // No function tables registered → no handler found → should error.
    let result = crate::seh::dispatch_hardware_fault(
        &mut engine,
        &mut state,
        wie_cpu::exception_code::ACCESS_VIOLATION,
        0x0, // fault at address 0
    );
    assert!(
        result.is_err(),
        "unhandled hardware fault should return error"
    );
}

// ── Phase 0: handle disjointness ─────────────────────────────────
//
// Ensures dynamically-allocated handle bases seeded in WindowState do not
// overlap the FAKE handle ranges used by USER32/GDI32 stubs.
//
// FAKE handles live in ranges:
//   USER32: 0x6600_0000..0x6601_xxxx
//   GDI32:  0x6800_0000..0x6800_xxxx
//
// Seeded bases live in:
//   window_handle:  0x6610_0000
//   menu_handle:    0x6620_0000
//   hook_handle:    0x6630_0000
//   atoms:          0xC000
//   timer_id:       1
//   (future GDI DC: 0x6810_0000, bitmap: 0x6820_0000)

#[test]
fn test_fake_handle_disjointness() {
    let ws = WindowState::default();

    // USER32 window handle range (0x6610_0000+) must not overlap FAKE range (0x6600_xxxx)
    assert!(
        ws.next_window_handle.as_u64() >= 0x0000_0000_6610_0000,
        "window handle base collides with FAKE range"
    );

    // Menu/hook bases
    assert!(
        ws.next_menu_handle.as_u64() >= 0x0000_0000_6620_0000,
        "menu handle base collides with FAKE range"
    );
    assert!(
        ws.next_windows_hook_handle.as_u64() >= 0x0000_0000_6630_0000,
        "hook handle base collides with FAKE range"
    );

    // Atoms in user-atom range (0xC000-0xFFFF)
    assert!(
        ws.next_window_class_atom >= 0xC000,
        "class atom base collides with FAKE range"
    );
    assert!(
        ws.next_global_atom >= 0xC000,
        "global atom base collides with FAKE range"
    );

    // Timer ID is trivially disjoint
    assert_eq!(ws.next_timer_id, 1, "timer ID must be 1");
}

// ── Windows-fidelity batch (fix-28): BM_* / focus / erase / tracking ──

/// A button child of a parent with a guest WndProc, for control tests.
fn push_button_pair(state: &mut WinApiState) -> (u64, u64) {
    let parent = 0x6610_0001_u64;
    let button = 0x6610_0002_u64;
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
        handle: crate::handles::Hwnd::from(button),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        control_text: "OK".to_owned(),
        menu_handle: 7,
        visible: true,
        width: 40,
        height: 20,
        ..Default::default()
    });
    (parent, button)
}

#[test]
fn test_bm_click_delivers_command_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, button) = push_button_pair(&mut state);

    // SendMessage(button, BM_CLICK): the host control WndProc must bridge
    // WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the parent's WndProc.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_CLICK,
        0,
        0,
    );
    let error = result.expect_err("BM_CLICK must bridge a WM_COMMAND callback");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 7
        ),
        "BM_CLICK must bridge WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the \
             parent WndProc, got {signal:?}"
    );
}

#[test]
fn test_space_keyboard_activates_focused_button() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, button) = push_button_pair(&mut state);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(button);

    // WM_KEYDOWN VK_SPACE on the focused button presses it.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_SPACE,
        0,
    )
    .expect("keydown handled")
    .expect("some result");
    assert_eq!(r, 0);
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(button))
            .is_some_and(|w| w.flags.contains(WindowFlags::PRESSED)),
        "space keydown must press the focused button"
    );

    // WM_KEYUP VK_SPACE releases and delivers BN_CLICKED to the parent.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYUP,
        crate::user32::VK_SPACE,
        0,
    );
    let error = result.expect_err("space keyup must click the button");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent && request.word_parameter == 7
        ),
        "space keyup must deliver WM_COMMAND(7) to the parent, got {signal:?}"
    );

    // Space on a NON-focused button is ignored (no press, no click).
    state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_SPACE,
        0,
    )
    .expect("non-focused keydown unhandled");
    assert!(r.is_none(), "non-focused button must not react to space");
}

#[test]
fn test_bm_get_set_state_roundtrip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, button) = push_button_pair(&mut state);

    // Fresh button: not pressed, not focused.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_GETSTATE,
        0,
        0,
    )
    .expect("getstate ok")
    .expect("some result");
    assert_eq!(r, 0);

    // BM_SETSTATE(TRUE) returns the previous (0) and presses.
    let prev = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_SETSTATE,
        1,
        0,
    )
    .expect("setstate ok")
    .expect("some result");
    assert_eq!(prev, 0);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_GETSTATE,
        0,
        0,
    )
    .expect("getstate ok")
    .expect("some result");
    assert_eq!(r, crate::user32::BST_PUSHED);

    // BM_SETSTATE(FALSE) returns 1 (was pressed) and releases.
    let prev = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_SETSTATE,
        0,
        0,
    )
    .expect("setstate ok")
    .expect("some result");
    assert_eq!(prev, 1);
}

#[test]
fn edit_message_on_a_button_does_not_touch_edit_state() {
    use crate::user32::controls::ControlState;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, button) = push_button_pair(&mut state);

    // EM_SETSEL is an EDIT-only message: a BUTTON must not handle it...
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::EM_SETSEL.as_u32(),
        2,
        4,
    )
    .expect("dispatch");
    assert!(r.is_none(), "a Button must not handle EM_SETSEL");

    // ... and no EDIT-typed state may exist for the button (reading
    // `caret` on the entry would be a compile error anyway).
    let entry = state
        .window_state()
        .control_states
        .get(&crate::handles::Hwnd::from(button));
    assert!(
        matches!(entry, None | Some(ControlState::Button { .. })),
        "EM_SETSEL on a Button must not create EDIT state, got {entry:?}"
    );
}

#[test]
fn test_track_mouse_event_arms_and_cancels() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let win = 0x6610_0001_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win),
        title: "Tracked".to_owned(),
        width: 100,
        height: 100,
        ..Default::default()
    });
    let tme = 0x5000_u64;
    engine
        .mem_map(tme, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map tme struct");
    // TRACKMOUSEEVENT: cbSize=24, dwFlags=TME_HOVER|TME_LEAVE, hwndTrack=win.
    engine
        .mem_write(tme, &24_u32.to_le_bytes())
        .expect("cbSize");
    engine
        .mem_write(tme + 4, &3_u32.to_le_bytes())
        .expect("flags");
    engine
        .mem_write(tme + 8, &win.to_le_bytes())
        .expect("hwndTrack");

    write_regs(&mut engine, tme, 0, 0, 0, 0);
    let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("TrackMouseEvent");
    assert_eq!(r.return_value, 1);
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("tracked window exists")
            .mouse_tracking,
        "TME must arm tracking on the target window"
    );

    // TME_CANCEL releases the tracking request.
    engine
        .mem_write(tme + 4, &0x8000_0000_u32.to_le_bytes())
        .expect("cancel flags");
    write_regs(&mut engine, tme, 0, 0, 0, 0);
    let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("TrackMouseEvent cancel");
    assert_eq!(r.return_value, 1);
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("tracked window exists")
            .mouse_tracking,
        "TME_CANCEL must clear tracking"
    );
}

#[test]
fn test_erase_background_fills_class_brush_and_clears_flag() {
    let mut state = default_winapi_state();
    // Register a class with the classic (HBRUSH)(COLOR_WINDOW + 1) stock
    // brush (6 = COLOR_WINDOW + 1) — the white background.
    let atom = crate::user32::register_window_class(
        &mut state,
        WindowClassRecord {
            atom: 0,
            class_name: "EraseClass".to_owned(),
            window_proc: 0x7000_0000,
            style: 0,
            instance_handle: 0,
            icon_handle: 0,
            cursor_handle: 0,
            background_brush: 6,
            small_icon_handle: 0,
            menu_name: 0,
            unicode: false,
        },
    )
    .expect("register class");
    let win = 0x6610_0001_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win),
        class_atom: u16::try_from(atom).unwrap_or(0),
        title: "Erase".to_owned(),
        width: 64,
        height: 32,
        invalidated: true,
        flags: WindowFlags::ERASE_BACKGROUND,
        ..Default::default()
    });

    assert!(
        crate::user32::message::erase_window_background(&mut state, win),
        "a class brush must erase the background"
    );
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("erased window exists")
            .flags
            .contains(WindowFlags::ERASE_BACKGROUND),
        "the erase must consume the pending-erase flag"
    );
    let surf = state
        .present()
        .surfaces
        .get(&crate::handles::Hwnd::from(win))
        .expect("erase surface exists");
    assert_eq!(surf.pixels.len(), 64 * 32);
    assert!(
        surf.pixels.iter().all(|&p| p == 0x00FF_FFFF),
        "COLOR_WINDOW brush must fill white (0RGB)"
    );

    // No class brush → no erase possible.
    let win2 = 0x6610_0002_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win2),
        title: "NoBrush".to_owned(),
        width: 8,
        height: 8,
        ..Default::default()
    });
    assert!(
        !crate::user32::message::erase_window_background(&mut state, win2),
        "no class brush → nothing to erase"
    );
}

#[test]
fn test_is_window_recognizes_real_windows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let parent = 0x6610_0001_u64;
    let child = 0x6610_0002_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(parent),
        title: "Parent".to_owned(),
        width: 100,
        height: 100,
        visible: true,
        flags: WindowFlags::ENABLED,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(child),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        title: "Child".to_owned(),
        visible: false,
        width: 40,
        height: 20,
        ..Default::default()
    });

    // IsWindow: real windows (including children) are valid.
    for handle in [parent, child] {
        write_regs(&mut engine, handle, 0, 0, 0, 0);
        let r = user32::handle_is_window(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindow");
        assert_eq!(r.return_value, 1, "IsWindow({handle:#x})");
    }
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, 0);
    let r = user32::handle_is_window(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindow");
    assert_eq!(r.return_value, 0, "unknown handle is not a window");

    // IsWindowVisible: parent visible, child hidden.
    write_regs(&mut engine, parent, 0, 0, 0, 0);
    let r = user32::handle_is_window_visible(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowVisible");
    assert_eq!(r.return_value, 1);
    write_regs(&mut engine, child, 0, 0, 0, 0);
    let r = user32::handle_is_window_visible(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowVisible");
    assert_eq!(r.return_value, 0);

    // IsWindowEnabled: child disabled, parent enabled.
    write_regs(&mut engine, child, 0, 0, 0, 0);
    let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowEnabled");
    assert_eq!(r.return_value, 0);
    write_regs(&mut engine, parent, 0, 0, 0, 0);
    let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowEnabled");
    assert_eq!(r.return_value, 1);
}

#[test]
fn test_is_dialog_message_enter_resolves_default_button() {
    use crate::user32::{VK_RETURN, WM_KEYDOWN};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let dialog = 0x6610_0001_u64;
    let ok_button = 0x6610_0002_u64;
    let cancel_button = 0x6610_0003_u64;
    {
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(dialog),
            dialog_proc: 0x7000_0001,
            title: "Dlg".to_owned(),
            width: 160,
            height: 60,
            visible: true,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(ok_button),
            parent_handle: crate::handles::Hwnd::from(dialog),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "OK".to_owned(),
            menu_handle: 1,
            visible: true,
            width: 60,
            height: 20,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(cancel_button),
            parent_handle: crate::handles::Hwnd::from(dialog),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "Cancel".to_owned(),
            menu_handle: 2,
            visible: true,
            width: 60,
            height: 20,
            ..Default::default()
        });
    }
    // OK is the BS_DEFPUSHBUTTON default; focus sits on Cancel.
    state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(ok_button))
        .or_insert_with(|| crate::user32::controls::ControlClassKind::Button.new_state())
        .set_default_push(true);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);

    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg");
    let dispatch = |engine: &mut IcedCpu, state: &mut WinApiState| {
        engine
            .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
            .expect("message");
        engine
            .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
            .expect("wParam");
        write_regs(engine, dialog, msg_va, 0, 0, 0x3000);
        let result = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
            engine,
            test_environment(),
            state,
        ));
        let error = result.expect_err("Enter must be consumed by the dialog");
        error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal")
            .clone()
    };

    // Enter with a default push button activates IT (id 1), not the
    // focused Cancel (id 2).
    let signal = dispatch(&mut engine, &mut state);
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.message == 0x0111 && request.word_parameter == 1
        ),
        "Enter must activate the BS_DEFPUSHBUTTON (id 1), got {signal:?}"
    );

    // Without a default button, Enter falls back to the focused button.
    state
        .window_state()
        .control_states
        .get_mut(&crate::handles::Hwnd::from(ok_button))
        .expect("ok state")
        .set_default_push(false);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);
    let signal = dispatch(&mut engine, &mut state);
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.word_parameter == 2
        ),
        "Enter with no default must activate the focused button (id 2), got {signal:?}"
    );

    // Neither a default nor a focused button: Enter is NOT consumed
    // (IsDialogMessage returns FALSE, caller dispatches normally).
    state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
    engine
        .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
        .expect("message");
    engine
        .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
        .expect("wParam");
    write_regs(&mut engine, dialog, msg_va, 0, 0, 0x3000);
    let r = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsDialogMessage completes");
    assert_eq!(
        r.return_value, 0,
        "Enter with no button target must not be consumed"
    );
}

#[test]
fn test_sys_color_mapping() {
    // COLOR_WINDOW (5) → white; COLOR_BTNFACE (15) → gray; these drive the
    // WM_ERASEBKGND class-brush fill and GetSysColor.
    assert_eq!(crate::user32::window::sys_color(5), 0x00FF_FFFF);
    assert_eq!(crate::user32::window::sys_color(15), 0x00F0_F0F0);
    assert_eq!(crate::user32::window::sys_color(0), 0x00C8_C8C8);
}

// ── Windows-fidelity tier (★15): EDIT caret/selection + LISTBOX selection ──

/// An EDIT child of a parent with a guest WndProc, for control tests.
fn push_edit_pair(state: &mut WinApiState) -> (u64, u64) {
    let parent = 0x6610_0011_u64;
    let edit = 0x6610_0012_u64;
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
        handle: crate::handles::Hwnd::from(edit),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::Edit),
        control_text: "hello".to_owned(),
        menu_handle: 12,
        visible: true,
        width: 120,
        height: 20,
        ..Default::default()
    });
    (parent, edit)
}

/// A multiline EDIT child (creation style carries `ES_MULTILINE`) holding the
/// given text — the Task 2.1 line-model fixture. Uses its own handle range so
/// a test can pair it with a `push_edit_pair` single-line EDIT.
fn push_multiline_edit(state: &mut WinApiState, text: &str) -> (u64, u64) {
    let parent = 0x6610_0015_u64;
    let edit = 0x6610_0016_u64;
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
        handle: crate::handles::Hwnd::from(edit),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::Edit),
        control_text: text.to_owned(),
        style: crate::user32::controls::ES_MULTILINE,
        menu_handle: 12,
        visible: true,
        width: 120,
        height: 20,
        ..Default::default()
    });
    (parent, edit)
}

/// A multiline EDIT created through the REAL `create_window_record` path —
/// the same call `CreateWindowExW` makes, so the `ES_MULTILINE` style arrives
/// in `dwStyle` and lands on the `WindowRecord` before any message touches the
/// control. `create_window_record` bumps `next_window_handle` (the default
/// test state starts it at 0x6610_0000), so the returned handle never
/// collides with the hardcoded fixtures above.
fn push_multiline_edit_real(state: &mut WinApiState) -> u64 {
    let (hwnd, _, _) = crate::user32::create_window_record(
        state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("EDIT".to_owned()),
            title: String::new(),
            style: crate::user32::controls::ES_MULTILINE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 120,
            height: 60,
        },
        true,
    )
    .expect("create edit record");
    assert_ne!(hwnd, 0, "create_window_record must allocate an edit handle");
    hwnd
}

#[test]
fn test_edit_multiline_real_creation_enter_inserts_newline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // The first touch is a real WM_CHAR through dispatch_control_proc: the
    // control state seeds from the WindowRecord's creation style, so Enter
    // must insert a `\n` (ES_MULTILINE) rather than the single-line no-op.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        crate::user32::VK_RETURN, // 0x0D
        0,
    )
    .expect("char ok")
    .expect("some result");
    assert!(
        control_text(&state, edit).contains('\n'),
        "Enter must insert \\n in a real-created multiline EDIT, got {:?}",
        control_text(&state, edit)
    );
    let ui = control_ui(&state, edit);
    assert_ne!(
        ui.style_bits & crate::user32::controls::ES_MULTILINE,
        0,
        "the seeded EDIT state must carry ES_MULTILINE from the creation style"
    );
}

#[test]
fn test_edit_multiline_style_survives_control_state_seed_order() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // Poison the seed: `control_state_mut`'s Edit path seeds via
    // `ControlClassKind::Edit.new_state()` → `new_edit_state(0)`, so a message
    // routed through it FIRST leaves the state with style 0 — the race the
    // user hit when an early control message touched the edit before typing
    // ever reached it. The WM_CHAR that follows must still insert the newline:
    // the style capture must not depend on which seeder ran first. (The
    // Task 2.5 mouse arms are NOT a poison source — they seed through
    // `edit_state_mut` with the window's real style.)
    state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(edit))
        .or_insert_with(|| crate::user32::controls::ControlClassKind::Edit.new_state());
    assert_eq!(
        control_ui(&state, edit).style_bits,
        0,
        "precondition: the control_state_mut seed carries style 0"
    );

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        crate::user32::VK_RETURN,
        0,
    )
    .expect("char ok")
    .expect("some result");
    assert!(
        control_text(&state, edit).contains('\n'),
        "Enter must still insert \\n after a style-0 control_state seed, got {:?}",
        control_text(&state, edit)
    );
    assert_ne!(
        control_ui(&state, edit).style_bits & crate::user32::controls::ES_MULTILINE,
        0,
        "the style-0 seed must be healed to the window's creation style"
    );
}

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

// ── L3 wake-on-destroy: DestroyWindow of a top-level requests a host sync ──

/// Install a wake spy: a flag the stored wake callback flips when fired.
fn wake_spy(state: &mut WinApiState) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fired = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&fired);
    state.present().wake = Some(Box::new(move || {
        flag.store(true, Ordering::SeqCst);
    }));
    fired
}

/// Destroy `hwnd` through the real handler entry; returns the LRESULT.
fn destroy_window(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64) -> u64 {
    write_regs(engine, hwnd, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "DestroyWindow")
        .expect("DestroyWindow must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(&mut HandlerContext::new(engine, default_env(), state), id)
        .expect("DestroyWindow must dispatch");
    r.return_value
}

#[test]
fn test_destroy_window_top_level_requests_host_sync() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let fired = wake_spy(&mut state);

    // A top-level window (no parent, no guest WndProc): DestroyWindow takes
    // the direct-removal path and must wake the host so the stale winit
    // window is dropped (a destroyed top-level publishes no frame).
    let (top, _, _) = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
            title: String::new(),
            style: 0,
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
    .expect("create top-level");

    let result = destroy_window(&mut engine, &mut state, top);
    assert_eq!(result, 1, "DestroyWindow of a known window succeeds");
    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        "destroying a top-level window must request a host sync"
    );
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .any(|w| w.handle == crate::handles::Hwnd::from(top)),
        "the top-level record must be removed"
    );
}

#[test]
fn test_destroy_window_child_does_not_request_host_sync() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let fired = wake_spy(&mut state);

    // A child window lives inside its parent's surface — the host has no
    // window of its own to drop, so no sync is requested.
    let (top, _, _) = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiClass".to_owned()),
            title: String::new(),
            style: 0,
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
    .expect("create parent");
    let (child, _, _) = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("GuiChild".to_owned()),
            title: String::new(),
            style: crate::user32::WS_CHILD,
            extended_style: 0,
            parent_handle: top,
            menu_handle: 0,
            instance_handle: 0,
            x: 10,
            y: 10,
            width: 100,
            height: 50,
        },
        true,
    )
    .expect("create child");

    let result = destroy_window(&mut engine, &mut state, child);
    assert_eq!(result, 1, "DestroyWindow of a child succeeds");
    assert!(
        !fired.load(std::sync::atomic::Ordering::SeqCst),
        "destroying a child window must NOT request a host sync"
    );
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .any(|w| w.handle == crate::handles::Hwnd::from(child)),
        "the child record must be removed"
    );
}

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

/// Test-only projection of a control's editable bits (window record
/// interaction flags + per-kind state), so assertions can read `caret` /
/// `sel_index` / … without matching on the variant.
#[derive(Debug, Clone, Default)]
struct ControlUiSnapshot {
    pressed: bool,
    focused: bool,
    default_push: bool,
    items: Vec<String>,
    caret: usize,
    sel_start: usize,
    sel_end: usize,
    goal_column: Option<usize>,
    sel_index: i32,
    style_bits: u32,
    limit: usize,
    modified: bool,
    first_visible_line: usize,
    first_visible_column: usize,
    dragging_scrollbar: bool,
    tab_stops: Vec<u16>,
    caret_on: bool,
    invalid_rows: crate::user32::controls::EditInvalidation,
    last_paint_rows: usize,
    part_rights: Vec<i32>,
    part_texts: Vec<String>,
}

impl ControlUiSnapshot {
    fn of(state: &WinApiState, hwnd: u64) -> Self {
        use crate::user32::controls::ControlState;
        let mut snap = Self::default();
        if let Some(ws) = state.try_window_state() {
            if let Some(window) = ws
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            {
                snap.pressed = window.flags.contains(WindowFlags::PRESSED);
                snap.focused = window.flags.contains(WindowFlags::FOCUSED);
            }
            match ws.control_states.get(&crate::handles::Hwnd::from(hwnd)) {
                Some(ControlState::Button { default_push, .. }) => {
                    snap.default_push = *default_push;
                }
                Some(ControlState::Edit {
                    caret,
                    sel_start,
                    sel_end,
                    goal_column,
                    style_bits,
                    limit,
                    modified,
                    first_visible_line,
                    first_visible_column,
                    scrollbar_drag,
                    tab_stops,
                    caret_on,
                    invalid_rows,
                    last_paint_rows,
                    ..
                }) => {
                    snap.caret = *caret;
                    snap.sel_start = *sel_start;
                    snap.sel_end = *sel_end;
                    snap.goal_column = *goal_column;
                    snap.style_bits = *style_bits;
                    snap.limit = *limit;
                    snap.modified = *modified;
                    snap.first_visible_line = *first_visible_line;
                    snap.first_visible_column = *first_visible_column;
                    snap.dragging_scrollbar = scrollbar_drag.is_some();
                    snap.tab_stops = tab_stops.clone();
                    snap.caret_on = *caret_on;
                    snap.invalid_rows = *invalid_rows;
                    snap.last_paint_rows = *last_paint_rows;
                }
                Some(ControlState::ListBox {
                    items, sel_index, ..
                }) => {
                    snap.items = items.clone();
                    snap.sel_index = *sel_index;
                }
                Some(ControlState::ComboBox { items, .. }) => {
                    snap.items = items.clone();
                }
                Some(ControlState::StatusBar {
                    part_rights,
                    part_texts,
                }) => {
                    snap.part_rights = part_rights.clone();
                    snap.part_texts = part_texts.clone();
                }
                Some(ControlState::Static { .. }) | None => {}
            }
        }
        snap
    }
}

/// The projected control UI state for a window (defaults when never touched).
fn control_ui(state: &WinApiState, hwnd: u64) -> ControlUiSnapshot {
    ControlUiSnapshot::of(state, hwnd)
}

/// The control text of a window (EM_GETTEXT-side read for assertions).
fn control_text(state: &WinApiState, hwnd: u64) -> String {
    state
        .try_window_state()
        .expect("window state")
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map_or_else(String::new, |w| w.control_text.clone())
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
fn test_edit_em_setsel_getsel_packing() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // EM_SETSEL(2, 4) → selection [2, 4), caret at the end edge (4).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETSEL returns TRUE");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 4, 4));

    // EM_GETSEL (no pointers) returns MAKELONG(start, end) = 2 | 4 << 16.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0,
        0,
    )
    .expect("getsel ok")
    .expect("some result");
    assert_eq!(r, 0x0004_0002, "EM_GETSEL packs MAKELONG(start, end)");
}

#[test]
fn test_edit_em_setsel_negative_selects_all() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // EM_SETSEL(0, -1) → select everything: [0, len).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        u64::from(u32::MAX),
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0,
        0,
    )
    .expect("getsel ok")
    .expect("some result");
    assert_eq!(r, 0x0005_0000, "select-all = MAKELONG(0, 5)");
}

#[test]
fn test_edit_em_getsel_writes_output_pointers() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
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

    // EM_GETSEL with output pointers at 0x3000 (start) / 0x3004 (end).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0x3000,
        0x3004,
    )
    .expect("getsel ok")
    .expect("some result");
    let mut start = [0u8; 4];
    let mut end = [0u8; 4];
    engine.mem_read(0x3000, &mut start).expect("read start");
    engine.mem_read(0x3004, &mut end).expect("read end");
    assert_eq!(
        u32::from_le_bytes(start),
        1,
        "wParam pointer receives start"
    );
    assert_eq!(u32::from_le_bytes(end), 3, "lParam pointer receives end");
}

#[test]
fn test_edit_wm_char_inserts_at_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Type 'X' at caret 0 → "Xhello", caret advances to 1.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Xhello");
    assert_eq!(control_ui(&state, edit).caret, 1);

    // Home + End, then 'Y' appends at the end → "XhelloY".
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('Y')),
        0,
    )
    .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "XhelloY");
    assert_eq!(control_ui(&state, edit).caret, 7);

    // Enter does not change the text and delivers nothing.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect("enter ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "XhelloY");
}

#[test]
fn test_edit_char_replaces_selection_and_delivers_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_edit_pair(&mut state);

    // Select "ell" (chars 1..4) of "hello".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");

    // Typing 'X' replaces [1, 4) → "hXo", clears the selection.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    );
    let error = result.expect_err("WM_CHAR must deliver EN_CHANGE to the parent");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0300_000C
        ),
        "WM_CHAR must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );

    assert_eq!(control_text(&state, edit), "hXo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));
}

#[test]
fn test_edit_backspace_and_delete_at_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Home + Right + Right (caret 2), Backspace → deletes the char before
    // the caret ('e', index 1) → "hllo", caret back at 1.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 2);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x08,
        0,
    )
    .expect_err("backspace delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hllo");
    assert_eq!(control_ui(&state, edit).caret, 1);

    // VK_DELETE at caret 1 deletes 'l' (index 1) → "hlo".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect_err("delete delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hlo");

    // Delete past the end: no text change, no notification.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect("delete at end ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "hlo");
}

#[test]
fn test_edit_arrow_keys_move_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Home → caret 0.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);

    // Right → caret 1; End → caret 5 (len of "hello"); Left → 4.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 1);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    assert_eq!(control_ui(&state, edit).caret, 4);

    // Left at the start is a no-op (stays 0 after Home).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

#[test]
fn test_edit_shift_arrow_extends_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // End (no shift) → caret 5, no selection; then hold Shift.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    let held = state.window_state().keyboard_state.get(0x10) | 0x80;
    state.window_state().keyboard_state.set(0x10, held); // VK_SHIFT held

    // Shift+Left selects the last char: [4, 5), caret 4.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 5, 4));

    // Shift+Left again extends: [3, 5), caret 3.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (3, 5, 3));

    // Release Shift; Right collapses the selection and moves the caret.
    let released = state.window_state().keyboard_state.get(0x10) & !0x80;
    state.window_state().keyboard_state.set(0x10, released);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 4, 4));
}

// ── Task 2.3: multiline EDIT keyboard navigation (vertical moves, line-aware
// Home/End, goal-column memory, page keys) ──

/// Press `vk` on `hwnd` via the control dispatch. A caret-movement key that
/// actually moved the caret answers with the EN_HSCROLL/EN_VSCROLL control
/// signal (the F5 status-bar refresh — the caret mutation already happened
/// before the notification was raised); a key at the document edge is a no-op
/// and answers `Some(0)`.
fn press_key(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64, vk: u64) {
    let result = crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        hwnd,
        crate::user32::WM_KEYDOWN,
        vk,
        0,
    );
    match result {
        Err(error) => {
            assert!(
                error
                    .downcast_ref::<WinApiControlSignal>()
                    .is_some_and(|signal| {
                        matches!(
                            signal,
                            WinApiControlSignal::GuestCallbackRequested { request }
                                if request.message == 0x0111
                                    && (request.word_parameter >> 16 == 0x0601
                                        || request.word_parameter >> 16 == 0x0602)
                        )
                    }),
                "a moved navigation key must deliver EN_HSCROLL or EN_VSCROLL, \
                 got {error:?}"
            );
        }
        Ok(value) => assert_eq!(value, Some(0), "a no-op key answers 0"),
    }
}

#[test]
fn test_edit_multiline_up_down_moves_between_lines() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");
    // "ab\ncd": line 0 = chars 0..2, line 1 = chars 3..5. Caret 1 is column 1
    // of line 0; VK_DOWN keeps the column on line 1 → caret 4 (3 + 1).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 1);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (4, Some(1)));
    // VK_UP returns to the same column on line 0.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (1, Some(1)));
    // Horizontal movement clears the goal column.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (2, None));
}

#[test]
fn test_edit_multiline_up_down_column_memory() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    // Lines: "ab" 0..2, "cd" 3..5, "efgh" 6..10. EM_SETSEL(9, 9) → caret 9 =
    // column 3 of the long line.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        9,
        9,
    )
    .expect("setsel ok")
    .expect("some result");
    // Up to "cd" (len 2): the goal column 3 clamps to 2 → caret 3 + 2 = 5.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Up to "ab" (len 2): still clamped → caret 0 + 2 = 2.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    assert_eq!(control_ui(&state, edit).caret, 2);
    // Down returns to the goal column 3 on "cd" → caret 3 + 2 = 5.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Down again on the long line: the remembered column 3 → caret 6 + 3 = 9.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (9, Some(3)));
}

#[test]
fn test_edit_multiline_home_end_line_aware() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    // Home/End are LINE-aware: from caret 0, End → line 0's end (2), not the
    // document end (10).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 2);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
    // Caret at line 1's end (5): Home → line 1 start (3), End → 5, NOT the
    // document end (10).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        5,
        5,
    )
    .expect("setsel ok")
    .expect("some result");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 3);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Ctrl+Home / Ctrl+End are document-wide.
    state.window_state().keyboard_state.set(0x11, 0x80); // VK_CONTROL held
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 10);
    state.window_state().keyboard_state.set(0x11, 0);
}

#[test]
fn test_edit_multiline_pgup_pgdn_move_a_page() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // The page heuristic is the client height / 16 px default line height;
    // an 80 px tall control gives a 5-line page.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }
    // PgDn from line 0 → line 5 (char index 10); PgDn again clamps to the
    // last line 9 (index 18).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_NEXT);
    assert_eq!(control_ui(&state, edit).caret, 10);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_NEXT);
    assert_eq!(control_ui(&state, edit).caret, 18);
    // PgUp steps back a page → line 4 (index 8), then line 0; past the top
    // is a no-op.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 8);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

#[test]
fn test_edit_shift_up_down_extends_selection_across_lines() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        5,
        5,
    )
    .expect("setsel ok")
    .expect("some result");
    state.window_state().keyboard_state.set(0x10, 0x80); // VK_SHIFT held
    // Shift+Down: caret 5 (line 1, col 2) → line 2, goal 2 → caret 8,
    // selection [5, 8).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 8, 8));
    // Shift+Up back to the anchor collapses the selection.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
    // Shift+Up again extends upward across the line break: [2, 5), caret 2.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 5, 2));
    // Shift+Down returns the caret to the anchor (5) and collapses the
    // selection — Windows EDIT anchor semantics, not a re-extension.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
    // Shift+Down again extends downward from the anchor across the break.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 8, 8));
    // Release Shift: Up collapses the selection and moves the caret.
    state.window_state().keyboard_state.set(0x10, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
}

#[test]
fn test_edit_single_line_vertical_keys_noop() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"
    // Vertical keys do not move the single-line caret; Home/End keep their
    // document-wide meaning (identical to line-aware on a single line).
    for vk in [
        crate::user32::VK_UP,
        crate::user32::VK_DOWN,
        crate::user32::VK_PRIOR,
        crate::user32::VK_NEXT,
    ] {
        press_key(&mut engine, &mut state, edit, vk);
        assert_eq!(control_ui(&state, edit).caret, 0);
    }
    let ui = control_ui(&state, edit);
    assert_eq!(ui.goal_column, None, "single-line edits never set a goal");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

// ── Task 2.1: multiline EDIT text model + EM_* state messages ──

#[test]
fn test_edit_wm_char_enter_multiline_inserts_newline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab");

    // End + WM_CHAR 0x0D on a multiline EDIT appends '\n'.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect_err("multiline Enter inserts '\n' and delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "ab\n");
    assert_eq!(control_ui(&state, edit).caret, 3);

    // The single-line EDIT keeps the historical no-op.
    let (_, single) = push_edit_pair(&mut state);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        single,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect("single-line enter ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, single), "hello");
}

#[test]
fn test_edit_em_limitext_caps_insertion() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello", len 5

    // EM_LIMITTEXT(5) == the current length: typing at the cap is a no-op.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LIMITTEXT,
        5,
        0,
    )
    .expect("limitext ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_LIMITTEXT returns TRUE");
    assert_eq!(control_ui(&state, edit).limit, 5);
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_GETLIMITTEXT,
            0,
            0,
        )
        .expect("getlimitext ok")
        .expect("some result"),
        5
    );

    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect("at-cap typing ok")
    .expect("some result");
    assert_eq!(r, 0, "insertion beyond the limit must be ignored");
    assert_eq!(control_text(&state, edit), "hello");
    assert!(
        !control_ui(&state, edit).modified,
        "a blocked insert must not dirty the modify flag"
    );

    // Deletion is never capped: backspace removes a char.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x08,
        0,
    )
    .expect_err("backspace delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hell");
    assert!(
        control_ui(&state, edit).modified,
        "a real deletion must dirty the modify flag"
    );

    // EM_REPLACESEL truncates a too-long replacement to the remaining room
    // (limit 5, selecting 'h' leaves room for 5 - 3 = 2 chars).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        1,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "PQRST");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    )
    .expect_err("EM_REPLACESEL delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "PQell");
}

#[test]
fn test_edit_em_line_messages_on_multiline_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // EM_GETLINECOUNT: '\n' separates lines; "ab\ncd" has 2.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINECOUNT,
        0,
        0,
    )
    .expect("linecount ok")
    .expect("some result");
    assert_eq!(r, 2);

    // Windows semantics: an empty multiline edit still reports 1 line. Uses a
    // push_edit_pair edit (0x6610_0012) so it does not collide with the
    // fixture's fixed multiline handle.
    let (_, empty_edit) = push_edit_pair(&mut state);
    {
        let ws = state.window_state();
        let window = ws
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(empty_edit))
            .expect("empty edit window");
        window.style |= crate::user32::controls::ES_MULTILINE;
        window.control_text = "".to_owned();
    }
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        empty_edit,
        crate::user32::EM_GETLINECOUNT,
        0,
        0,
    )
    .expect("empty linecount ok")
    .expect("some result");
    assert_eq!(r, 1, "an empty multiline edit has one (empty) line");

    // EM_LINEFROMCHAR: '\n' belongs to the line it terminates.
    for (index, expected) in [(0_u64, 0_u64), (2, 0), (3, 1), (4, 1)] {
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_LINEFROMCHAR,
            index,
            0,
        )
        .expect("linefromchar ok")
        .expect("some result");
        assert_eq!(r, expected, "LINEFROMCHAR({index})");
    }

    // EM_LINEINDEX: the char index of each line start; -1 out of range.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        0,
        0,
    )
    .expect("lineindex0 ok")
    .expect("some result");
    assert_eq!(r, 0);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        1,
        0,
    )
    .expect("lineindex1 ok")
    .expect("some result");
    assert_eq!(r, 3, "line 1 starts after \"ab\" + the '\n'");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        2,
        0,
    )
    .expect("lineindex2 ok")
    .expect("some result");
    assert_eq!(r, u64::MAX, "out-of-range line returns -1");

    // EM_LINELENGTH: chars in the line, excluding its '\n'.
    for (index, expected) in [(0_u64, 2_u64), (3, 2)] {
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_LINELENGTH,
            index,
            0,
        )
        .expect("linelength ok")
        .expect("some result");
        assert_eq!(r, expected, "LINELENGTH({index})");
    }
    // wParam == -1: the length of the caret's line (caret starts at 0).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINELENGTH,
        u64::from(u32::MAX),
        0,
    )
    .expect("linelength caret ok")
    .expect("some result");
    assert_eq!(r, 2, "LINELENGTH(-1) uses the caret's line");

    // EM_GETLINE: the buffer's first WORD is the capacity (incl. the NUL);
    // the copy strips the line's '\n' and NUL-terminates.
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        0,
        0x4000,
    )
    .expect("getline0 ok")
    .expect("some result");
    assert_eq!(r, 2, "EM_GETLINE returns the char count");
    let mut line0 = [0_u8; 4];
    engine.mem_read(0x4000, &mut line0).expect("read line 0");
    assert_eq!(line0, *b"ab\0\0", "line 0 copies \"ab\" without the EOL");
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        1,
        0x4000,
    )
    .expect("getline1 ok")
    .expect("some result");
    assert_eq!(r, 2);
    let mut line1 = [0_u8; 4];
    engine.mem_read(0x4000, &mut line1).expect("read line 1");
    assert_eq!(line1, *b"cd\0\0", "line 1 copies \"cd\" without the EOL");
    // Out-of-range line → 0.
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        7,
        0x4000,
    )
    .expect("getline7 ok")
    .expect("some result");
    assert_eq!(r, 0, "out-of-range line copies nothing");
}

#[test]
fn test_edit_em_replacesel_replaces_selection_and_fires_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_edit_pair(&mut state); // "hello"

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "XY");
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    );
    let error = result.expect_err("EM_REPLACESEL must deliver EN_CHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0300_000C
        ),
        "EM_REPLACESEL must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );

    assert_eq!(control_text(&state, edit), "hXYo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (3, 3, 3));
}

#[test]
fn test_edit_em_scrollcaret_updates_first_visible_line() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nef");

    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETFIRSTVISIBLELINE,
        0,
        0,
    )
    .expect("firstvisible ok")
    .expect("some result");
    assert_eq!(r, 0, "fresh edit starts at the first line");

    // Caret to line 2 ('e', char index 6), then EM_SCROLLCARET brings that
    // line into view.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        6,
        6,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SCROLLCARET returns TRUE");
    assert_eq!(control_ui(&state, edit).first_visible_line, 2);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETFIRSTVISIBLELINE,
        0,
        0,
    )
    .expect("firstvisible ok")
    .expect("some result");
    assert_eq!(r, 2);
}

// ── Task 2.4: multiline-EDIT vertical scrolling (viewport, WM_VSCROLL, wheel,
// minimal EM_SCROLLCARET).

/// Resize the EDIT's client height so exactly `visible` rows fit at the
/// resolved default font's line height — the same 16 px font the scroll math
/// resolves, so the fixture's viewport matches the handler's.
fn set_edit_visible_rows(state: &mut WinApiState, hwnd: u64, visible: usize) {
    let line_h = {
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = font_engine
            .resolve(&crate::gdi32::FontKey::default(), 16)
            .map_or(16, |f| f.line_height());
        state.gdi_state().font_engine = font_engine;
        line_h
    };
    let height = i32::try_from(visible).unwrap_or(0).saturating_mul(line_h);
    let ws = state.window_state();
    if let Some(w) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
    {
        w.height = height;
    }
}

/// Dispatch WM_VSCROLL (with an SB_* code and thumb position) and return the
/// resulting first-visible-line offset.
fn vscroll_offset(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    edit: u64,
    code: u16,
    thumb: u16,
) -> usize {
    let wparam = u64::from(code) | (u64::from(thumb) << 16);
    let result = crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::wm::WinMsg::WM_VSCROLL.as_u32(),
        wparam,
        0,
    );
    match result {
        // The scroll did not move the offset (no notification).
        Ok(_) => {}
        // A real scroll delivers EN_VSCROLL to the parent (Task 3.1), which
        // surfaces as a GuestCallbackRequested signal — still a success.
        Err(error) => {
            let _ = error
                .downcast_ref::<WinApiControlSignal>()
                .expect("vscroll ok");
        }
    }
    control_ui(state, edit).first_visible_line
}

/// A WM_MOUSEWHEEL wParam whose high word carries the signed `delta`.
fn wheel_wparam(delta: i32) -> u64 {
    let hi = i16::try_from(delta).unwrap_or(0);
    u64::from(u16::from_le_bytes(hi.to_le_bytes())) << 16
}

/// Dispatch WM_MOUSEWHEEL with a signed delta and return the resulting
/// first-visible-line offset.
fn wheel_offset(engine: &mut IcedCpu, state: &mut WinApiState, edit: u64, delta: i32) -> usize {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEWHEEL.as_u32(),
        wheel_wparam(delta),
        0,
    )
    .expect("wheel ok")
    .expect("some result");
    control_ui(state, edit).first_visible_line
}

#[test]
fn test_edit_visible_line_count_and_clamp() {
    use crate::user32::controls::{clamp_scroll_offset, visible_line_count};
    // Floor division: a partial row at the bottom is clipped.
    assert_eq!(visible_line_count(48, 16), 3);
    assert_eq!(visible_line_count(40, 16), 2);
    assert_eq!(visible_line_count(16, 16), 1);
    // Degenerate metrics still show one row so the caret stays reachable.
    assert_eq!(visible_line_count(0, 16), 1);
    assert_eq!(visible_line_count(48, 0), 1);
    // The offset stays inside [0, total − visible]: 0 when the text fits.
    assert_eq!(clamp_scroll_offset(0, 5, 3), 0);
    assert_eq!(clamp_scroll_offset(2, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(4, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(99, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(3, 2, 3), 0);
}

#[test]
fn test_edit_wm_vscroll_scroll_codes() {
    // WM_VSCROLL scroll-bar codes (winuser.h) — the wParam low word.
    const SB_LINEUP: u16 = 0;
    const SB_LINEDOWN: u16 = 1;
    const SB_PAGEUP: u16 = 2;
    const SB_PAGEDOWN: u16 = 3;
    const SB_THUMBPOSITION: u16 = 4;
    const SB_THUMBTRACK: u16 = 5;
    const SB_TOP: u16 = 6;
    const SB_BOTTOM: u16 = 7;
    const SB_ENDSCROLL: u16 = 8;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    // 5 visual rows, 3 visible → the offset clamps to max(0, 5 − 3) = 2.
    set_edit_visible_rows(&mut state, edit, 3);

    // SB_TOP: first row; SB_BOTTOM: the last offset that keeps the final row
    // visible (NOT the last row itself).
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        2
    );

    // SB_LINEUP / SB_LINEDOWN step by one row, clamped at both ends.
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        1
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEUP, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        2
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        2
    ); // clamped

    // SB_PAGEUP / SB_PAGEDOWN move by the visible-row count (3).
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_PAGEDOWN, 0),
        2
    ); // 3 clamped
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_PAGEUP, 0),
        0
    );

    // SB_THUMBTRACK / SB_THUMBPOSITION jump to the high-word thumb position.
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBTRACK, 1),
        1
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBPOSITION, 2),
        2
    );

    // SB_ENDSCROLL is a no-op.
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_ENDSCROLL, 0),
        2
    );
}

#[test]
fn test_edit_wm_vscroll_short_text_stays_at_zero() {
    const SB_LINEDOWN: u16 = 1;
    const SB_THUMBTRACK: u16 = 5;
    const SB_BOTTOM: u16 = 7;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb");
    // Content (2 rows) fits entirely in a 3-row viewport: every code clamps
    // to 0.
    set_edit_visible_rows(&mut state, edit, 3);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBTRACK, 5),
        0
    );
}

#[test]
fn test_edit_em_scrollcaret_minimal_scroll() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    // 5 rows, 3 visible: the viewport shows rows [first, first + 3).
    set_edit_visible_rows(&mut state, edit, 3);

    // Caret BELOW the viewport (line 4, the last row): the offset advances
    // just enough to show it on the LAST visible row — NOT snapping it to
    // the top (the pre-Task-2.4 behavior).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        8, // 'e', line 4
        8,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        2,
        "caret below the viewport lands on the LAST visible row (4 − 3 + 1), not the top"
    );

    // Caret ABOVE the viewport: scrolls back up to reveal it at the top.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2, // 'b', line 1
        2,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        1,
        "caret above the viewport scrolls back to it"
    );

    // Caret already visible: no movement.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        6, // 'd', line 3 — inside rows [1, 4)
        6,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        1,
        "a visible caret must not move the offset"
    );
}

#[test]
fn test_edit_wm_mousewheel_scrolls() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    set_edit_visible_rows(&mut state, edit, 3);

    // Positive delta (wheel away from the user) scrolls UP — already at the
    // top, so nothing moves.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, 120), 0);
    // A full notch scrolls 3 lines (the Windows default); clamped to 2.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -120), 2);
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -240), 2); // clamped
    // Back up: 2 − 3 → 0.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, 120), 0);
    // A partial notch (60 delta units) is below the 120-unit line threshold.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -60), 0);
}

// ── Task 2.2: pure multiline-EDIT layout helper (`layout_visible_lines`).

/// A row's (text, y) as a plain pair for compact assertions.
fn row_pair(row: &crate::user32::controls::VisibleSegment) -> (&str, i32) {
    (row.text.as_str(), row.y)
}

#[test]
fn test_edit_layout_wrap_off_one_row_per_logical_line() {
    // "ab\ncd\nef" without wrap: 3 logical lines, one row each, at
    // y = line × line_height, carrying the whole-text char offsets the
    // selection/caret math needs.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        0,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("cd", 16), ("ef", 32)]);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 5));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 8));
    assert!(
        rows.iter().all(|r| r.x == 0),
        "left-aligned rows start at 0"
    );
}

#[test]
fn test_edit_layout_wrap_splits_long_line_at_width() {
    // Wrap on, 8 px/char, 32 px column → 4 chars per visual row: "abcdef"
    // becomes "abcd" at y=0 and "ef" at y=16, with contiguous char offsets.
    let rows = crate::user32::controls::layout_visible_lines(
        "abcdef",
        32,
        16,
        0,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("abcd", 0), ("ef", 16)]);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
    assert_eq!((rows[1].char_start, rows[1].char_end), (4, 6));
}

#[test]
fn test_edit_layout_wrap_applies_per_logical_line() {
    // The wrap column resets between logical lines: "ab" fits untouched and
    // "cdef" wraps to 3 chars + 1 in the same 24 px (8 px/char) column.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncdef",
        24,
        16,
        0,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("cde", 16), ("f", 32)]);
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 7));
}

#[test]
fn test_edit_layout_blank_line_occupies_a_row() {
    // "ab\n\ncd" splits into 3 logical lines; the empty middle line still
    // occupies its vertical slot so the following text lands at y=32.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\n\ncd",
        80,
        16,
        0,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("", 16), ("cd", 32)]);
}

#[test]
fn test_edit_layout_first_visible_skips_rows_and_rebases_y() {
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        1,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("cd", 0), ("ef", 16)]);
}

#[test]
fn test_edit_layout_scroll_clamps_past_last_row() {
    // first_visible beyond the last row clamps to the last row: only "ef"
    // remains, at the top.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        5,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ef", 0)]);

    // The clamp counts VISUAL rows: wrapped lines make the last visual row
    // later than the last logical line.
    let wrapped = crate::user32::controls::layout_visible_lines(
        "abcdef",
        32,
        16,
        9,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let wrapped_shown: Vec<(&str, i32)> = wrapped.iter().map(row_pair).collect();
    assert_eq!(wrapped_shown, [("ef", 0)]);
}

#[test]
fn test_edit_layout_alignment_offsets_row_x() {
    // "ab" = 16 px in a 40 px column: ES_CENTER → x=12, ES_RIGHT → x=24.
    let centered = crate::user32::controls::layout_visible_lines(
        "ab",
        40,
        16,
        0,
        false,
        0x1,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(centered[0].x, 12);
    let right = crate::user32::controls::layout_visible_lines(
        "ab",
        40,
        16,
        0,
        false,
        0x2,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(right[0].x, 24);
    // A row wider than the column still starts at the left edge.
    let overwide = crate::user32::controls::layout_visible_lines(
        "abcdefgh",
        40,
        16,
        0,
        false,
        0x2,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(overwide[0].x, 0);
}

#[test]
fn test_edit_layout_empty_text_single_empty_row() {
    let rows =
        crate::user32::controls::layout_visible_lines("", 80, 16, 0, true, 0, &mut |_ch: char| {
            8_i32
        });
    assert_eq!(rows.len(), 1);
    assert_eq!(row_pair(&rows[0]), ("", 0));
}

#[test]
fn test_edit_em_pos_from_char_uses_font_line_height() {
    // EM_POSFROMCHAR answers y = line × the REAL resolved font line height
    // (the 16 px default control font), not the DIALOG_BASE_UNIT_Y constant.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        3, // 'c', line 1
        3,
    )
    .expect("setsel ok")
    .expect("some result");
    let ok = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        3,
        0x4000,
    )
    .expect("posfromchar ok")
    .expect("some result");
    assert_eq!(ok, 1, "EM_POSFROMCHAR returns TRUE for a valid index");
    let mut bytes = [0_u8; 8];
    engine.mem_read(0x4000, &mut bytes).expect("read point");
    let x = i32::from_le_bytes(bytes[0..4].try_into().expect("x"));
    let y = i32::from_le_bytes(bytes[4..8].try_into().expect("y"));
    assert_eq!(x, 0, "x stays 0 (per-glyph x is Task 2.5)");
    // The same 16 px default font the paint path resolves.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let line_h = font_engine
        .resolve(&crate::gdi32::FontKey::default(), 16)
        .expect("resolve default font")
        .line_height();
    state.gdi_state().font_engine = font_engine;
    assert_eq!(y, line_h, "line 1's y is one real line height");
}

#[test]
fn test_edit_em_modify_flags_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh edit is unmodified.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 0);

    // EM_SETMODIFY(1) → GETMODIFY 1; EM_SETMODIFY(0) → GETMODIFY 0.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETMODIFY,
        1,
        0,
    )
    .expect("setmodify1 ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 1);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETMODIFY,
        0,
        0,
    )
    .expect("setmodify0 ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 0);

    // Typing sets the flag again.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("typing delivers EN_CHANGE");
    assert!(
        control_ui(&state, edit).modified,
        "typing must set the modify flag"
    );
}

#[test]
fn test_edit_em_get_handle_caches_and_invalidates() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    // Prime the guest heap control block (bump cursor at 0x2000; the freelist
    // heads stay zeroed) so the LocalAlloc-style coherent allocation works —
    // the runtime seeds this block at session init.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("guest heap bump cursor");

    let first = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle ok")
    .expect("some result");
    assert_ne!(first, 0, "EM_GETHANDLE returns a guest buffer");
    // ANSI: the handle points at "hello" + NUL.
    let mut head = [0_u8; 6];
    engine
        .mem_read(first, &mut head)
        .expect("read handle buffer");
    assert_eq!(head, *b"hello\0", "handle buffer holds a copy of the text");

    // A second GETHANDLE with no intervening mutation reuses the cached
    // buffer instead of leaking a fresh allocation per call.
    let again = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle again ok")
    .expect("some result");
    assert_eq!(
        again, first,
        "repeat GETHANDLE without a text change must return the cached handle"
    );

    // A keystroke mutates the text → the cache clears and the next GETHANDLE
    // allocates a fresh buffer holding the new text.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("typing delivers EN_CHANGE");
    let fresh = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle after mutation ok")
    .expect("some result");
    assert_ne!(fresh, first, "a mutation must invalidate the cached handle");
    let mut head = [0_u8; 7];
    engine
        .mem_read(fresh, &mut head)
        .expect("read fresh buffer");
    assert_eq!(head, *b"helloX\0", "the fresh buffer holds the new text");

    // WM_SETTEXT also changes the text: the next GETHANDLE is fresh again.
    write_guest_ansi(&mut engine, 0x4000, "set");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    let after_settext = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle after settext ok")
    .expect("some result");
    assert_ne!(
        after_settext, fresh,
        "WM_SETTEXT must invalidate the cached handle"
    );
    let mut head = [0_u8; 4];
    engine
        .mem_read(after_settext, &mut head)
        .expect("read settext buffer");
    assert_eq!(head, *b"set\0", "the buffer holds the WM_SETTEXT text");
}

#[test]
fn test_edit_em_set_handle_adopts_guest_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    // Leave a selection behind so SETHANDLE's reset is observable.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "adopted");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETHANDLE,
        0,
        0x4000,
    )
    .expect("sethandle ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETHANDLE returns TRUE");
    assert_eq!(control_text(&state, edit), "adopted");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (0, 0, 0));

    // The adopted buffer becomes the cached GETHANDLE result (the text is
    // unchanged since the adoption, so no fresh allocation happens).
    let handle = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle ok")
    .expect("some result");
    assert_eq!(handle, 0x4000, "GETHANDLE returns the adopted buffer");
}

#[test]
fn test_edit_em_settabstops_stores_stops() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // Two explicit stops at 4 and 8 dialog units.
    engine
        .mem_write(0x4000, &4_u16.to_le_bytes())
        .expect("stop 0");
    engine
        .mem_write(0x4002, &8_u16.to_le_bytes())
        .expect("stop 1");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETTABSTOPS,
        2,
        0x4000,
    )
    .expect("settabstops ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETTABSTOPS returns TRUE");
    assert_eq!(control_ui(&state, edit).tab_stops, vec![4, 8]);

    // wParam 0 resets to the default tab stops (the stored list clears).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETTABSTOPS,
        0,
        0,
    )
    .expect("settabstops reset ok")
    .expect("some result");
    assert_eq!(r, 1);
    assert_eq!(
        control_ui(&state, edit).tab_stops,
        Vec::<u16>::new(),
        "wParam 0 restores default stops"
    );
}

#[test]
fn test_edit_em_posfromchar_basic_answer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // Char 3 ('c') sits on line 1 → y = one REAL resolved-font line height
    // (the same 16 px default control font the paint path uses), x = 0.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        3,
        0x4000,
    )
    .expect("posfromchar ok")
    .expect("some result");
    assert_eq!(r, 1, "valid char returns TRUE");
    assert_eq!(read_test_i32(&mut engine, 0x4000), 0, "x is 0");
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let line_h = font_engine
        .resolve(&crate::gdi32::FontKey::default(), 16)
        .expect("resolve the default control font")
        .line_height();
    state.gdi_state().font_engine = font_engine;
    assert_eq!(
        read_test_i32(&mut engine, 0x4004),
        line_h,
        "y = line × the font's line height"
    );

    // Out-of-range char → FALSE.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        100,
        0x4000,
    )
    .expect("posfromchar oob ok")
    .expect("some result");
    assert_eq!(r, 0, "invalid char returns FALSE");
}

#[test]
fn test_edit_em_selectiontype_basic_answers() {
    use crate::user32::controls::{SEL_EMPTY, SEL_MULTICHAR, SEL_MULTILINE, SEL_TEXT};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // No selection → SEL_EMPTY.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_EMPTY);

    // One char → SEL_TEXT.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        1,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT);

    // Two chars on one line → SEL_TEXT | SEL_MULTICHAR.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        2,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT | SEL_MULTICHAR);

    // "\nc" spans two lines' characters → SEL_TEXT | SEL_MULTICHAR |
    // SEL_MULTILINE (a selection ending exactly at a '\n' stays on that line).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT | SEL_MULTICHAR | SEL_MULTILINE);
}

// ── Task 2.6: EDIT undo + clipboard ─────────────────────────────────────

/// EM_CANUNDO through the control dispatch (0 = no undo pending).
fn can_undo(engine: &mut IcedCpu, state: &mut WinApiState, edit: u64) -> u64 {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::EM_CANUNDO,
        0,
        0,
    )
    .expect("canundo ok")
    .expect("some result")
}

/// `IsClipboardFormatAvailable(CF_TEXT)` through the full dispatch path.
fn clipboard_available(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    write_regs(engine, u64::from(crate::clipboard::CF_TEXT), 0, 0, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "IsClipboardFormatAvailable")
        .expect("IsClipboardFormatAvailable must resolve to a WinApiId");
    crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("IsClipboardFormatAvailable must dispatch")
    .return_value
}

#[test]
fn test_edit_em_canundo_tracks_insert_delete_replace() {
    // Insert: typing 'X' at the caret captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "a fresh edit has nothing to undo"
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "an insert captures an undo snapshot"
    );

    // Delete: VK_DELETE at the caret captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect_err("delete delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "a delete captures an undo snapshot"
    );

    // Replace: EM_REPLACESEL over a selection captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "Z");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    )
    .expect_err("replacesel delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "a replace captures an undo snapshot"
    );
}

#[test]
fn test_edit_em_undo_restores_text_caret_and_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Select "ell" [1,4) (the caret lands at 4) and type 'X' → "hXo".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hXo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));

    // EM_UNDO reverts the single operation: text, caret AND selection all
    // return to the pre-mutation state ("hello", caret 4, selection [1,4)).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (4, 1, 4));

    // Single-level buffer: the undo consumed the snapshot, so a second
    // EM_UNDO does nothing (no redo) and EM_CANUNDO is false again.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect("second undo ok")
    .expect("some result");
    assert_eq!(r, 0, "EM_UNDO with an empty buffer returns FALSE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(can_undo(&mut engine, &mut state, edit), 0);
}

#[test]
fn test_edit_wm_undo_matches_em_undo() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // The Edit menu's Undo command sends WM_UNDO to the focused edit — it
    // must behave exactly like EM_UNDO.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Xhello");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_UNDO,
        0,
        0,
    )
    .expect_err("WM_UNDO delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(control_ui(&state, edit).caret, 0);
    assert_eq!(can_undo(&mut engine, &mut state, edit), 0);
}

#[test]
fn test_edit_em_emptyundobuffer_clears() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(can_undo(&mut engine, &mut state, edit), 1);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_EMPTYUNDOBUFFER,
        0,
        0,
    )
    .expect("emptyundobuffer ok")
    .expect("some result");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "EM_EMPTYUNDOBUFFER discards the snapshot"
    );
}

/// EM_CANUNDO parity through the real dispatch path, sending the RAW guest
/// values (winuser.h): the host used to declare EM_CANUNDO as 0x00A6, so a
/// guest's 0x00C6 never matched a dispatch arm and fell through to an
/// unhandled zero. SetWindowText must also clear the undo buffer (Windows
/// clears it on any program-set text).
#[test]
fn test_edit_em_canundo_parity_and_settext_clears_undo() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // The raw 0x00C6 must reach the EM_CANUNDO arm (a fresh edit has nothing
    // to undo). Pre-fix this fell through: dispatch returned None.
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "a fresh edit has nothing to undo"
    );

    // An editable change (typing) captures a snapshot → TRUE.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        1,
        "an insert is undoable"
    );

    // EM_UNDO restores the text and consumes the snapshot → FALSE.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "undo consumed the snapshot"
    );

    // SetWindowText (the raw WM_SETTEXT = 0x000C a SendMessage carries) must
    // clear the undo buffer: edit again so a snapshot is pending, then set
    // the text — EM_CANUNDO goes false.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('Y')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Yhello");
    write_guest_ansi(&mut engine, 0x4000, "fresh");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        0x000C,
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    assert_eq!(control_text(&state, edit), "fresh");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "SetWindowText clears the undo buffer"
    );
}

#[test]
fn test_edit_wm_copy_stores_selection_on_clipboard() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // A fresh session has an empty clipboard.
    assert!(!state.clipboard().has_text());

    // WM_COPY without a selection is a no-op.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert!(!state.clipboard().has_text());

    // Select "ell" [1,4) and copy → the clipboard holds "ell".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(state.clipboard().text(), Some("ell"));

    // Copy does not change the text or the selection.
    assert_eq!(control_text(&state, edit), "hello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (1, 4));
}

#[test]
fn test_edit_wm_paste_inserts_clipboard_text_replacing_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Seed the clipboard with the full text, then move to the start.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        u64::from(u32::MAX),
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);

    // Paste at the caret (no selection) inserts the clipboard text.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect_err("paste delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hellohello");
    assert_eq!(control_ui(&state, edit).caret, 5);

    // Paste over a selection replaces it: [1,4) "ell" of "hellohello" is
    // replaced by "hello" → "h" + "hello" + "ohello" = "hhelloohello".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect_err("paste delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hhelloohello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (6, 6, 6));
}

#[test]
fn test_edit_wm_paste_empty_clipboard_is_noop() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh session: nothing on the clipboard, so paste changes nothing.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect("paste ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "an empty-clipboard paste is not a mutation"
    );
}

#[test]
fn test_edit_wm_cut_copies_and_deletes() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Select "ell" [1,4) and cut → the clipboard holds "ell" and the text
    // loses it ("ho"), with the caret collapsing at the deletion point.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CUT,
        0,
        0,
    )
    .expect_err("cut delivers EN_CHANGE");
    assert_eq!(state.clipboard().text(), Some("ell"));
    assert_eq!(control_text(&state, edit), "ho");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (1, 1, 1));

    // The cut is a mutation: EM_UNDO restores the pre-cut state.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
}

#[test]
fn test_edit_wm_clear_deletes_without_writing_clipboard() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Put the full text on the clipboard first (a copy).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        u64::from(u32::MAX),
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert!(state.clipboard().has_text());

    // Select "ell" [1,4) and clear → the text loses it ("ho") and the
    // clipboard is EMPTIED — Windows' edit control calls EmptyClipboard, so
    // the deleted text is never written to the clipboard (that is what
    // distinguishes CLEAR from CUT) and IsClipboardFormatAvailable goes false.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect_err("clear delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "ho");
    assert!(
        !state.clipboard().has_text(),
        "WM_CLEAR must empty the clipboard"
    );

    // Clearing with no selection is a no-op (no text change, no notify).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect("clear ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "ho");
}

#[test]
fn test_is_clipboard_format_available_tracks_clipboard_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh session: no text on the clipboard → FALSE.
    assert_eq!(clipboard_available(&mut engine, &mut state), 0);

    // WM_COPY the selection → TRUE.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        u64::from(u32::MAX),
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(clipboard_available(&mut engine, &mut state), 1);

    // WM_CLEAR empties the clipboard → FALSE again.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect_err("clear delivers EN_CHANGE");
    assert_eq!(clipboard_available(&mut engine, &mut state), 0);
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

/// A top-level window with an EDIT child, for paint tests.
fn push_edit_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0021_u64;
    let edit = 0x6610_0022_u64;
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
        handle: crate::handles::Hwnd::from(edit),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 10,
        width: 60,
        height: 20,
        control_kind: Some(crate::user32::controls::ControlClassKind::Edit),
        control_text: "hello".to_owned(),
        visible: true,
        ..Default::default()
    });
    (top, edit)
}

/// A top-level window with a MULTILINE EDIT child (three short non-wrapping
/// lines in a 120×60 client), for the row-level invalidation tests.
fn push_multiline_edit_pair(state: &mut WinApiState) -> (u64, u64) {
    let (top, edit) = push_edit_paint_pair(state);
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.style = crate::user32::WS_CHILD
                | crate::user32::WS_VISIBLE
                | crate::user32::controls::ES_MULTILINE;
            w.width = 120;
            w.height = 60;
            w.control_text = "alpha\nbeta\ngamma".to_owned();
        }
    }
    (top, edit)
}

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

/// A top-level window with a BUTTON child (100×30 at (10,10)), for the
/// label-control region tests.
fn push_button_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0031_u64;
    let button = 0x6610_0032_u64;
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
        handle: crate::handles::Hwnd::from(button),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 10,
        width: 100,
        height: 30,
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        control_text: "OK".to_owned(),
        visible: true,
        ..Default::default()
    });
    (top, button)
}

/// A top-level window with a STATIC child (140×20 at (10,50)), for the
/// label-control region tests.
fn push_static_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0041_u64;
    let label = 0x6610_0042_u64;
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
        handle: crate::handles::Hwnd::from(label),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 50,
        width: 140,
        height: 20,
        control_kind: Some(crate::user32::controls::ControlClassKind::Static),
        control_text: "Ready".to_owned(),
        visible: true,
        ..Default::default()
    });
    (top, label)
}

/// The first paint of a BUTTON covers its whole rect — the surface behind a
/// never-painted control is undefined, so the region cannot be narrowed.
#[test]
fn test_button_first_paint_reports_the_full_control_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
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

/// A BUTTON caption change (WM_SETTEXT) must report ONLY the caption band —
/// the union of the old and new caption rects — instead of the full control
/// rect, and every pixel change must land inside that region.
#[test]
fn test_button_caption_change_paints_only_the_caption_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    // First paint (full); capture the frame with the old caption "OK".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
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

    // Change the caption to a longer one, then repaint.
    write_guest_ansi(&mut engine, 0x4000, "Start!");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
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

    let region = after.region.expect("a caption change is a partial repaint");
    let control = crate::gdi32::IRect {
        left: 10,
        top: 10,
        right: 110,
        bottom: 40,
    };
    assert!(
        region.left >= control.left
            && region.top >= control.top
            && region.right <= control.right
            && region.bottom <= control.bottom,
        "the region must stay inside the control, got {region:?}"
    );
    assert!(
        region != control,
        "a caption change must not report the full control rect, got {region:?}"
    );
    assert!(
        region.height() < control.height(),
        "the region is a single caption band, not the full face, got {region:?}"
    );

    // Every pixel diff between the two frames lies inside the region.
    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
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
    assert!(diffs > 0, "the caption change must repaint pixels");
}

/// A BUTTON pressed-state change must report ONLY the face rect — the
/// interior inside the 1 px border — because the border color does not
/// change.
#[test]
fn test_button_press_reports_only_the_face_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    // First paint (full) so the border is established.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();

    // Press the button (a click in its client area) and repaint.
    let lparam = u64::from(10_u16) | (u64::from(10_u16) << 16);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_LBUTTONDOWN,
        0,
        lparam,
    )
    .expect("press ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
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
        Some(crate::gdi32::IRect {
            left: 11,
            top: 11,
            right: 109,
            bottom: 39,
        }),
        "a press repaints only the face inside the 1 px border"
    );
}

/// A STATIC caption change must report ONLY the caption band, not the whole
/// label rect.
#[test]
fn test_static_caption_change_paints_only_the_caption_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, label) = push_static_paint_pair(&mut state);

    // First paint (full); capture the frame with the old caption "Ready".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
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

    write_guest_ansi(&mut engine, 0x4000, "Scanning drive C");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
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

    let region = after.region.expect("a caption change is a partial repaint");
    let control = crate::gdi32::IRect {
        left: 10,
        top: 50,
        right: 150,
        bottom: 70,
    };
    assert!(
        region.left >= control.left
            && region.top >= control.top
            && region.right <= control.right
            && region.bottom <= control.bottom,
        "the region must stay inside the control, got {region:?}"
    );
    assert!(
        region != control,
        "a caption change must not report the full control rect, got {region:?}"
    );
    assert!(
        region.height() < control.height(),
        "the region is a single caption band, not the whole label, got {region:?}"
    );

    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
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
    assert!(diffs > 0, "the caption change must repaint pixels");
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

// ── Task 2.5: EDIT mouse caret placement, drag selection, double-click ──

/// A client point packed into an lParam (x = low word, y = high word).
fn mouse_lparam(x: u16, y: u16) -> u64 {
    u64::from(x) | (u64::from(y) << 16)
}

/// The client x of char `index`'s glyph-cell start — the 2 px left margin
/// plus the summed advances of the preceding characters (the paint's caret x).
fn char_cell_left(state: &mut WinApiState, hwnd: u64, index: usize) -> i32 {
    let text = control_text(state, hwnd);
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16).expect("default font");
    let x = 2_i32.saturating_add(font_engine.text_advance(&resolved, &default_key, &text, index));
    state.gdi_state().font_engine = font_engine;
    x
}

/// The char index the EDIT hit-test resolves for a client point, computed
/// with the real default font — the same metrics the dispatch handlers use,
/// so dispatch-level assertions track the paint exactly. `wrap` must match
/// the fixture's style (single-line edits: false; the ES_MULTILINE fixture
/// without WS_HSCROLL wraps).
fn edit_char_at(state: &mut WinApiState, hwnd: u64, x: i32, y: i32, wrap: bool) -> usize {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    let (text, width) = {
        let ws = state.try_window_state().expect("window state");
        let w = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .expect("edit record");
        (w.control_text.clone(), w.width)
    };
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16).expect("default font");
    let line_h = resolved.line_height();
    let advance = &mut |ch: char| font_engine.char_advance(&resolved, &default_key, ch);
    let index = edit_char_index_at_point(
        &text,
        x,
        y,
        &HitTestLayout {
            wrap_width: width.saturating_sub(4),
            line_height: line_h,
            first_visible: 0,
            wrap,
            alignment: 0,
        },
        advance,
    );
    state.gdi_state().font_engine = font_engine;
    index
}

/// Dispatch one mouse message to a control and expect a plain handled result
/// (no guest callback — pure selection changes must not bridge WM_COMMAND).
fn dispatch_mouse(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    x: u16,
    y: u16,
) -> u64 {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        hwnd,
        message,
        0,
        mouse_lparam(x, y),
    )
    .expect("mouse message must be handled, not bridge a guest callback")
    .expect("some result")
}

/// Release the button over an EDIT after a press, accepting the EN_VSCROLL
/// control signal a completed click navigation delivers (the F5 status-bar
/// caret refresh — the caret/selection already settled before it fired).
fn release_mouse(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64, x: u16, y: u16) {
    let result = crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        hwnd,
        crate::user32::wm::WinMsg::WM_LBUTTONUP.as_u32(),
        0,
        mouse_lparam(x, y),
    );
    match result {
        Err(error) => {
            assert!(
                error
                    .downcast_ref::<WinApiControlSignal>()
                    .is_some_and(|signal| {
                        matches!(
                            signal,
                            WinApiControlSignal::GuestCallbackRequested { request }
                                if request.message == 0x0111
                                    && request.word_parameter >> 16 == 0x0602
                        )
                    }),
                "a completed click must deliver EN_VSCROLL, got {error:?}"
            );
        }
        Ok(value) => assert_eq!(value, Some(0), "a click release answers 0"),
    }
}

#[test]
fn test_edit_char_index_at_point_maps_x_to_char_cells() {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    // 8 px/char, no wrap, left-aligned: char i occupies the cell [8i, 8i+8);
    // the half-advance boundary puts the caret before a char when the click
    // is in its left half and after it in the right half (the paint's caret x
    // is the summed advance of the preceding chars — this is the inverse).
    let idx = |x: i32| {
        edit_char_index_at_point(
            "hello world",
            x,
            0,
            &HitTestLayout {
                wrap_width: 80,
                line_height: 16,
                first_visible: 0,
                wrap: false,
                alignment: 0,
            },
            &mut |_| 8_i32,
        )
    };
    assert_eq!(idx(0), 0, "left margin → before the first char");
    assert_eq!(idx(6), 1, "right half of 'h' → after 'h'");
    assert_eq!(idx(10), 1, "left half of 'e' → before 'e'");
    assert_eq!(idx(14), 2, "right half of 'e' → after 'e'");
    // "hello world" is 11 chars = 88 px; a click past the last glyph clamps
    // to the end of the text.
    assert_eq!(idx(90), 11, "past the last glyph → end of text");
    assert_eq!(idx(-5), 0, "left of the text → before the first char");
}

#[test]
fn test_edit_char_index_at_point_maps_wrapped_visual_rows() {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    // 8 px/char in a 32 px column → 4 chars per visual row: "abcdef" wraps to
    // "abcd" at y=0 and "ef" at y=16 (the same layout the paint draws).
    let idx = |x: i32, y: i32| {
        edit_char_index_at_point(
            "abcdef",
            x,
            y,
            &HitTestLayout {
                wrap_width: 32,
                line_height: 16,
                first_visible: 0,
                wrap: true,
                alignment: 0,
            },
            &mut |_| 8_i32,
        )
    };
    // Row 0 ("abcd", chars 0..4): the half-advance boundary applies within
    // the row-local cell.
    assert_eq!(idx(2, 0), 0, "first row, left half of 'a'");
    assert_eq!(idx(6, 0), 1, "first row, right half of 'a'");
    // A click just past the wrap boundary (the end of 'd') lands at char 4 —
    // the start of the second visual row.
    assert_eq!(idx(34, 0), 4, "wrap boundary → 'e'");
    // Row 1 ("ef", chars 4..6) at y=16: the row for y is the visual row, and
    // the char index is the whole-text offset, not a row-local one.
    assert_eq!(idx(10, 16), 5, "second row, right half of 'e'");
    assert_eq!(idx(19, 16), 6, "second row, past 'f' → end of text");
    // A click above the first row clamps to it.
    assert_eq!(idx(2, -1), 0, "above the text → first row start");
}

#[test]
fn test_edit_mouse_down_places_caret_and_captures() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello", width 120, single-line

    let (x, y) = (20_i32, 10_i32);
    let expected = edit_char_at(&mut state, edit, x, y, false);
    let r = dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        u16::try_from(x).unwrap_or(0),
        u16::try_from(y).unwrap_or(0),
    );
    assert_eq!(r, 0);

    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (expected, expected, expected),
        "a click must collapse the selection at the hit-tested char"
    );
    assert!(ui.focused, "a click focuses the edit");
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::from(edit),
        "a pressed edit holds the mouse capture for the drag"
    );
}

#[test]
fn test_edit_mouse_drag_extends_selection_from_anchor() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    let y = 10_u16;
    let anchor = 2_usize;
    let down_x = u16::try_from(char_cell_left(&mut state, edit, anchor)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        down_x,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));

    // Drag right to char 5: selection [anchor, current], caret at current.
    let x5 = u16::try_from(char_cell_left(&mut state, edit, 5)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x5,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 5, 5));

    // Drag back past the anchor to char 1: the anchor stays at 2.
    let x1 = u16::try_from(char_cell_left(&mut state, edit, 1)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x1,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (1, 2, 1));

    // ...and forward again to char 4: still anchored at 2.
    let x4 = u16::try_from(char_cell_left(&mut state, edit, 4)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 4, 4));

    // Release: the capture drops, the selection stays finalized, and the
    // completed click delivers EN_VSCROLL to the parent.
    release_mouse(&mut engine, &mut state, edit, x4, y);
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::NULL,
        "button-up must release the edit's mouse capture"
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (2, 4));

    // A hover move after the release must not touch the selection.
    let x3 = u16::try_from(char_cell_left(&mut state, edit, 3)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x3,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (2, 4), "hover must not select");
}

#[test]
fn test_edit_mouse_dblclk_selects_whitespace_word() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .control_text = "hello world foo".to_owned();

    // Double-click inside "world" (chars 6..11): the whole word selects and
    // the caret lands at its end.
    let x = u16::try_from(char_cell_left(&mut state, edit, 8)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x,
        10,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.sel_start, ui.sel_end, ui.caret),
        (6, 11, 11),
        "double-click must select the whole word under the click"
    );
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::from(edit),
        "the dblclk press also captures for a subsequent drag"
    );

    // A double-click on the whitespace between words (char 5) selects nothing.
    let x = u16::try_from(char_cell_left(&mut state, edit, 5)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x,
        10,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.sel_start, ui.sel_end),
        (5, 5),
        "a double-click on whitespace must not select a word"
    );
}

#[test]
fn test_edit_mouse_click_in_wrapped_text_selects_the_visual_row() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, &"m".repeat(40)); // wraps
    let line_h = {
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = font_engine
            .resolve(&crate::gdi32::FontKey::default(), 16)
            .map_or(16, |f| f.line_height());
        state.gdi_state().font_engine = font_engine;
        line_h
    };

    // A click in the SECOND visual row (below the first wrap row) must land
    // on a later char than the same x in the first row.
    let x = 5_i32;
    let first_row = edit_char_at(&mut state, edit, x, line_h / 2, true);
    let second_row = edit_char_at(&mut state, edit, x, line_h + line_h / 2, true);
    assert!(
        second_row > first_row,
        "the wrapped second row must hold later chars ({second_row} > {first_row})"
    );

    let r = dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        u16::try_from(x).unwrap_or(0),
        u16::try_from(line_h + line_h / 2).unwrap_or(0),
    );
    assert_eq!(r, 0);
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (second_row, second_row, second_row),
        "the click caret must land on the hit-tested visual-row char"
    );
}

/// A multiline EDIT whose client shows `visible` full rows plus a `partial` px
/// strip: the partial strip is the only in-client region that maps to a row
/// past the viewport (the first pixels of the row after the last visible one),
/// so a click there exercises the click→scroll-caret handoff.
fn set_edit_viewport_with_partial_strip(
    state: &mut WinApiState,
    hwnd: u64,
    visible: usize,
    partial: i32,
) {
    set_edit_visible_rows(state, hwnd, visible);
    let ws = state.window_state();
    if let Some(w) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
    {
        w.height = w.height.saturating_add(partial);
    }
}

#[test]
fn test_edit_mouse_click_scrolls_caret_row_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "aaaa\nbbbb\ncccc\ndddd\neeee\nffff");
    // 3 full rows + a 4 px partial strip: a click in the strip's first pixel
    // lands on visual row 3 (off-screen) and must scroll it onto the last
    // visible row.
    set_edit_viewport_with_partial_strip(&mut state, edit, 3, 4);
    let height = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .height;
    let y = u16::try_from(height.saturating_sub(4)).unwrap_or(0);

    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        ui.first_visible_line, 1,
        "a click on the off-viewport row must scroll it into view"
    );
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (15, 15, 15),
        "the click caret lands at the start of the clicked row ('dddd')"
    );
}

#[test]
fn test_edit_mouse_dblclk_scrolls_word_row_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "aaaa\nbbbb\ncccc\ndddd\neeee\nffff");
    set_edit_viewport_with_partial_strip(&mut state, edit, 3, 4);
    let height = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .height;
    let y = u16::try_from(height.saturating_sub(4)).unwrap_or(0);

    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        ui.first_visible_line, 1,
        "a double-click on the off-viewport row must scroll the word into view"
    );
    assert_eq!(
        (ui.sel_start, ui.sel_end, ui.caret),
        (15, 19, 19),
        "the double-click selects the whole off-screen word"
    );
}

#[test]
fn test_edit_mouse_selection_fires_no_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    let original = control_text(&state, edit);

    let y = 10_u16;
    let x_a = u16::try_from(char_cell_left(&mut state, edit, 1)).unwrap_or(0);
    let x_b = u16::try_from(char_cell_left(&mut state, edit, 4)).unwrap_or(0);
    // A full click-drag-release cycle leaves the text and modify flag
    // untouched — EN_CHANGE fires only for text mutations. The release does
    // bridge EN_VSCROLL (the F5 status-bar caret refresh), which is a
    // navigation notification, not a text change.
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        x_a,
        y,
    );
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x_b,
        y,
    );
    release_mouse(&mut engine, &mut state, edit, x_b, y);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x_a,
        y,
    );
    assert_eq!(
        control_text(&state, edit),
        original,
        "selection must not mutate the control text"
    );
    assert!(
        !control_ui(&state, edit).modified,
        "selection must not set EM_GETMODIFY"
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
                .saturating_mul(usize::try_from(after.width).unwrap_or(0))
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
    assert_eq!(frame.pixels.len(), 200 * 100);
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
    assert_eq!(frame.pixels.len(), 200 * 100);

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
    assert_eq!(frame.pixels.len(), 200 * 100);
}

// ── P3 D3D9 software-render handlers ────────────────────────────────

/// D3DCLEAR_TARGET (d3d9.rs keeps these private).
const D3DCLEAR_TARGET: u32 = 0x0000_0001;
/// D3DTS_WORLD.
const D3DTS_WORLD: u32 = 256;
/// D3DPT_TRIANGLELIST.
const D3DPT_TRIANGLELIST: u32 = 4;

#[test]
fn test_d3d9_caps_declare_pixel_shader_pipeline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let caps_va = 0x5000_u64;
    // GetDeviceCaps(this, adapter=0, type=HAL, pCaps).
    write_regs(&mut engine, 1, 0, 1, caps_va, 0);
    assert_return_value!(
        d3d9::handle_get_device_caps(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0 // D3D_OK
    );
    let mut read_u32_at = |offset: u64| -> u32 {
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(caps_va + offset, &mut bytes)
            .expect("read caps field");
        u32::from_le_bytes(bytes)
    };
    // Caps honesty: the ps_2_0 interpreter is implemented, so the caps
    // report D3DPS_VERSION(2,0) and PixelShader1xMaxValue 1.0; the vertex
    // stage is still FFP (vertex shader execution is not yet implemented),
    // so VertexShaderVersion stays 0 and MaxVertexShaderConst reports the
    // vs_2_0 constant file.
    assert_eq!(read_u32_at(196), 0, "VertexShaderVersion must be 0.0");
    assert_eq!(
        read_u32_at(200),
        256,
        "MaxVertexShaderConst must be 256 (vs_2_0 constant file)"
    );
    assert_eq!(
        read_u32_at(204),
        0xFFFF_0200,
        "PixelShaderVersion must be D3DPS_VERSION(2,0)"
    );
    assert_eq!(
        read_u32_at(208),
        1.0_f32.to_bits(),
        "PixelShader1xMaxValue must be 1.0"
    );
}

#[test]
fn test_d3d9_clear_fills_whole_backbuffer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
    }
    // Clear(this, Count=0, pRects=NULL, Flags=TARGET, Color=0xFFC80000).
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let back = &state.d3d9().d3d9_backbuffer;
    assert_eq!(back.len(), 12);
    for (index, pixel) in back.iter().enumerate() {
        assert_eq!(
            *pixel, 0x00_C8_00_00,
            "backbuffer pixel {index} must be the clear color (0RGB)"
        );
    }
    assert_eq!(
        state.d3d9().d3d9_dirty,
        None,
        "Clear marks the frame full-dirty"
    );
}

#[test]
fn test_d3d9_clear_fills_only_requested_rects() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
    }
    // One D3DRECT {1,1,3,3} at 0x6000.
    let rect_va = 0x6000_u64;
    for (i, v) in [1_i32, 1, 3, 3].iter().enumerate() {
        engine
            .mem_write(
                rect_va + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write rect field");
    }
    write_regs(&mut engine, 1, 1, rect_va, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_00_00_FF_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let back = &state.d3d9().d3d9_backbuffer;
    // Row y, column x → back[y * 4 + x].
    assert_eq!(
        back.first().copied(),
        Some(0),
        "outside rect stays unchanged"
    );
    assert_eq!(
        back.get(4 + 2).copied(),
        Some(0x00_00_00_FF),
        "inside rect filled"
    );
    assert_eq!(
        back.get(8 + 2).copied(),
        Some(0x00_00_00_FF),
        "inside rect filled"
    );
    assert_eq!(
        back.get(4).copied(),
        Some(0),
        "outside rect stays unchanged"
    );
}

#[test]
fn test_d3d9_begin_end_scene_flags() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_scene_active,
        crate::state::SceneState::Active
    );
    // A second BeginScene inside a scene fails.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_scene_active,
        crate::state::SceneState::Inactive
    );
    // EndScene outside a scene fails.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c
    );
}

#[test]
fn test_d3d9_set_transform_and_viewport_state() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A known 4x4 matrix (column-major) at 0x6000.
    let matrix_va = 0x6000_u64;
    let mut matrix = [0.0_f32; 16];
    matrix[0] = 2.0;
    matrix[5] = 3.0;
    matrix[10] = 0.5;
    matrix[15] = 1.0;
    for (i, value) in matrix.iter().enumerate() {
        engine
            .mem_write(
                matrix_va + u64::try_from(i).unwrap_or(0) * 4,
                &value.to_le_bytes(),
            )
            .expect("write matrix element");
    }
    write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_world_matrix.map(f32::to_bits),
        matrix.map(f32::to_bits),
        "SetTransform must store the matrix unchanged"
    );

    // SetViewport: D3DVIEWPORT9 {X,Y,Width,Height,MinZ,MaxZ} at 0x6400.
    let vp_va = 0x6400_u64;
    for (i, v) in [4_u32, 5, 100, 50].iter().enumerate() {
        engine
            .mem_write(vp_va + u64::try_from(i).unwrap_or(0) * 4, &v.to_le_bytes())
            .expect("write viewport field");
    }
    engine
        .mem_write(vp_va + 16, &0.25_f32.to_le_bytes())
        .expect("write MinZ");
    engine
        .mem_write(vp_va + 20, &0.75_f32.to_le_bytes())
        .expect("write MaxZ");
    write_regs(&mut engine, 1, vp_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_viewport(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let (vp_x, vp_y, vp_w, vp_h, vp_min_z, vp_max_z) = state.d3d9().d3d9_viewport;
    assert_eq!((vp_x, vp_y, vp_w, vp_h), (4, 5, 100, 50));
    // Bit-exact float compare (0.25/0.75 are exactly representable).
    assert_eq!(vp_min_z.to_bits(), 0.25_f32.to_bits());
    assert_eq!(vp_max_z.to_bits(), 0.75_f32.to_bits());

    // GetViewport writes it back.
    let out_va = 0x6800_u64;
    write_regs(&mut engine, 1, out_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_viewport(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out = [0_u8; 24];
    engine
        .mem_read(out_va, &mut out)
        .expect("read viewport out");
    assert_eq!(u32::from_le_bytes(out[0..4].try_into().expect("x")), 4);
    assert_eq!(u32::from_le_bytes(out[8..12].try_into().expect("w")), 100);
}

#[test]
fn test_d3d9_draw_primitive_up_rasterizes_triangle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 16;
        d3d.d3d9_backbuffer_height = 16;
        d3d.d3d9_backbuffer = vec![0_u32; 16 * 16];
        d3d.d3d9_viewport = (0, 0, 16, 16, 0.0, 1.0);
    }
    // FVF = XYZ | DIFFUSE.
    write_regs(&mut engine, 1, u64::from(0x0002_u32 | 0x0040_u32), 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_fvf(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // Clear to red first.
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // Full-frame triangle (NDC corners) with per-vertex colors.
    let data_va = 0x6000_u64;
    let vertices: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, 1.0, 0.0]];
    let colors: [u32; 3] = [0xFF_FF_00_00, 0xFF_00_FF_00, 0xFF_00_00_FF];
    for (i, vertex) in vertices.iter().enumerate() {
        for (j, component) in vertex.iter().enumerate() {
            engine
                .mem_write(
                    data_va
                        + u64::try_from(i).unwrap_or(0) * 16
                        + u64::try_from(j).unwrap_or(0) * 4,
                    &component.to_le_bytes(),
                )
                .expect("write vertex position");
        }
        engine
            .mem_write(
                data_va + u64::try_from(i).unwrap_or(0) * 16 + 12,
                &colors.get(i).copied().unwrap_or(0).to_le_bytes(),
            )
            .expect("write vertex color");
    }
    // DrawPrimitiveUP(this, TRIANGLELIST, 1, data, stride=16).
    write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 1, data_va, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &16_u64.to_le_bytes())
        .expect("write stride");
    assert_return_value!(
        d3d9::handle_draw_primitive_up(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // The triangle spans the bottom-left half (bounded by the diagonal
    // from (0,0) to (16,16)): inside pixels are colored, outside pixels
    // keep the clear red.
    let back = &state.d3d9().d3d9_backbuffer;
    let pixel = |x: u32, y: u32| {
        back.get(usize::try_from(y).unwrap_or(0) * 16 + usize::try_from(x).unwrap_or(0))
            .copied()
    };
    assert_eq!(
        pixel(14, 1),
        Some(0x00_C8_00_00),
        "pixels above the diagonal keep the clear color"
    );
    let red_channel = (pixel(1, 14).unwrap_or(0) >> 16) & 0xFF;
    let green_channel = (pixel(14, 14).unwrap_or(0) >> 8) & 0xFF;
    let blue_channel = pixel(1, 1).unwrap_or(0) & 0xFF;
    assert!(red_channel > 0xB0, "bottom-left corner red-dominant");
    assert!(green_channel > 0xB0, "bottom-right corner green-dominant");
    assert!(blue_channel > 0xB0, "top-left corner blue-dominant");
    // The dirty region covers the whole frame.
    assert_eq!(state.d3d9().d3d9_dirty, None, "draw keeps full-frame dirty");

    // EndScene then Present publishes the frame.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
}

#[test]
fn test_d3d9_present_publishes_surface_frame() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = (0_u32..12).collect();
        d3d.d3d9_present_hwnd = crate::handles::Hwnd::from(0x7777);
        d3d.d3d9_dirty = None;
    }
    // Unknown hwnd → window_client_size falls back to WindowState.
    state.window_state().window_width = 4;
    state.window_state().window_height = 3;

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(0x7777))
        .expect("Present must publish a SurfaceFrame");
    assert_eq!((frame.width, frame.height), (4, 3));
    assert_eq!(&frame.pixels[..], &(0_u32..12).collect::<Vec<u32>>()[..]);
}

// ── P4b texture handlers ───────────────────────────────────────────

/// D3DFMT_A8R8G8B8.
const D3DFMT_A8R8G8B8: u32 = 21;

#[test]
fn test_d3d9_texture_lock_unlock_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // The runtime seeds the guest heap bump cursor at session init; the
    // test heap control block starts zeroed, so seed it before allocating.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateTexture(2x2, levels=1, format=A8R8G8B8) → texture at 0x7000.
    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 1, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);
    assert_ne!(texture_va, 0, "CreateTexture must return an object");

    // GetSurfaceLevel(0) → surface at 0x7100.
    let pp_surface = 0x7100_u64;
    write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(surface_va, 0, "GetSurfaceLevel must return a surface");

    // LockRect → D3DLOCKED_RECT { Pitch, pBits } at 0x7200; write texels.
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, surface_va, locked_rect, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut pitch_bytes = [0_u8; 4];
    engine
        .mem_read(locked_rect, &mut pitch_bytes)
        .expect("read pitch");
    assert_eq!(u32::from_le_bytes(pitch_bytes), 8, "2x2 pitch must be 8");
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    assert_ne!(p_bits, 0, "LockRect must hand out a guest block");

    // 2x2 texels: red / green / blue / white (D3DCOLOR).
    let texels = [0xFFFF_0000_u32, 0xFF00_FF00, 0xFF00_00FF, 0xFFFF_FFFF];
    for (i, texel) in texels.iter().enumerate() {
        engine
            .mem_write(
                p_bits + u64::try_from(i).unwrap_or(0) * 4,
                &texel.to_le_bytes(),
            )
            .expect("write texel");
    }

    // UnlockRect → texels land in the host record.
    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(record.width, 2);
    assert_eq!(record.height, 2);
    assert_eq!(
        record.pixels,
        texels.to_vec(),
        "unlock must copy the texels back"
    );
    assert_eq!(record.locked_va, 0, "lock state cleared");

    // SetTexture(0, tex) → GetTexture(0) round-trip.
    write_regs(&mut engine, 1, 0, texture_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let out_texture = 0x7300_u64;
    write_regs(&mut engine, 1, 0, out_texture, 0, 0);
    assert_return_value!(
        d3d9::handle_get_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_texture, &mut out_bytes)
        .expect("read texture out");
    assert_eq!(u64::from_le_bytes(out_bytes), texture_va);

    // Release the texture: record gone, bindings cleared.
    write_regs(&mut engine, texture_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(!state.d3d9().d3d9_textures.contains_key(&texture_va));
    assert_eq!(state.d3d9().d3d9_texture_bindings[0], 0);
}

#[test]
fn test_d3d9_texture_unlock_with_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 1, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);

    let pp_surface = 0x7100_u64;
    write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);

    // Lock only the top-left texel: RECT {0,0,1,1} at 0x7400.
    let rect_ptr = 0x7400_u64;
    for (i, v) in [0_i32, 0, 1, 1].iter().enumerate() {
        engine
            .mem_write(
                rect_ptr + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write rect field");
    }
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, surface_va, locked_rect, rect_ptr, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    // The rect's pBits points at the rect top-left (the block start).
    assert_ne!(p_bits, 0);

    // Write the top-left texel (red); leave the rest of the block zero.
    engine
        .mem_write(p_bits, &0xFFFF_0000_u32.to_le_bytes())
        .expect("write texel");

    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(
        record.pixels.first().copied(),
        Some(0xFFFF_0000),
        "rect region texel copied back"
    );
    assert_eq!(
        record.pixels.get(1).copied(),
        Some(0),
        "outside the rect stays zero"
    );
    assert_eq!(record.pixels.get(2).copied(), Some(0));
    assert_eq!(record.pixels.get(3).copied(), Some(0));
}

#[test]
fn test_d3d9_depth_surface_create_bind_and_clear() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the texture round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateDepthStencilSurface(2x2, D16) → surface at 0x7000.
    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 80, 0); // r9 = D3DFMT_D16 = 80
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(
        surface_va, 0,
        "CreateDepthStencilSurface must return an object"
    );

    // Unsupported format fails honestly.
    write_regs(&mut engine, 1, 2, 2, 99, 0); // unknown format
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );

    // Bind it and clear the depth buffer to 0.25 (near is 0.0).
    write_regs(&mut engine, 1, surface_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // Clear(D3DCLEAR_ZBUFFER, z=0.25) — the Z arg is a f32 at [rsp+0x30].
    write_regs(&mut engine, 1, 0, 0, 2, 0); // flags = D3DCLEAR_ZBUFFER
    engine
        .mem_write(STACK_TOP + 0x30, &0.25_f32.to_bits().to_le_bytes())
        .expect("write clear z");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_depth_surfaces
        .get(&surface_va)
        .expect("depth record exists");
    assert_eq!(record.width, 2);
    assert_eq!(record.height, 2);
    assert_eq!(record.format, 80);
    assert_eq!(
        record.depth,
        vec![0.25; 4],
        "Clear(ZBUFFER) must fill the depth"
    );

    // GetDepthStencilSurface returns the binding.
    let out_surface = 0x7100_u64;
    write_regs(&mut engine, 1, out_surface, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_surface, &mut out_bytes)
        .expect("read out");
    assert_eq!(u64::from_le_bytes(out_bytes), surface_va);

    // Release the depth surface: record gone + binding cleared.
    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(!state.d3d9().d3d9_depth_surfaces.contains_key(&surface_va));
    assert_eq!(state.d3d9().d3d9_depth_stencil, 0, "release must unbind");
}

#[test]
fn test_d3d9_render_state_typed_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // SetRenderState(this, state, value) through the register ABI.
    let set = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32, value: u32| {
        write_regs(engine, 1, u64::from(state_id), u64::from(value), 0, 0);
        assert_return_value!(
            d3d9::handle_set_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    // GetRenderState(this, state, &out) returns the typed value.
    let get = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, u64::from(state_id), out, 0, 0);
        assert_return_value!(
            d3d9::handle_get_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetRenderState output");
        u32::from_le_bytes(bytes)
    };

    // The D3D9 fixed-function defaults match the fragment-stage fallbacks.
    let rs = state.d3d9();
    assert!(!rs.d3d9_render_state.alpha_blend_enable);
    assert!(rs.d3d9_render_state.z_write_enable);
    assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 0);
    assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
    assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 2);
    assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 1);
    assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

    // Set + struct check for every supported D3DRS_*.
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ALPHABLENDENABLE,
        1,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ZENABLE,
        1,
    );
    set(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC, 4);
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_SRCBLEND,
        5,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_DESTBLEND,
        6,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_BLENDOP,
        1,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ZWRITEENABLE,
        0,
    );
    let rs = state.d3d9();
    assert!(rs.d3d9_render_state.alpha_blend_enable);
    assert!(!rs.d3d9_render_state.z_write_enable);
    assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 1);
    assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
    assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 5);
    assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 6);
    assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

    // GetRenderState round-trips each supported state.
    assert_eq!(
        get(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ALPHABLENDENABLE
        ),
        1
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZENABLE),
        1
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC),
        4
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
        5
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_DESTBLEND),
        6
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_BLENDOP),
        1
    );
    assert_eq!(
        get(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ZWRITEENABLE
        ),
        0
    );

    // Unmodeled D3DRS_* are inert: setting them is a no-op and the read
    // falls back to 0 (D3D9's default for unused states).
    set(&mut engine, &mut state, 0x1FF, 7);
    assert_eq!(get(&mut engine, &mut state, 0x1FF), 0);
    // Unknown enum values round-trip through the raw register value.
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_SRCBLEND,
        0xDEAD,
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
        0xDEAD
    );
}

#[test]
fn test_d3d9_stage_state_typed_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // SetTextureStageState(this, stage=0, slot, value) via the register ABI.
    let set_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
        write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
        assert_return_value!(
            d3d9::handle_set_texture_stage_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    // SetSamplerState(this, sampler=0, slot, value) via the register ABI.
    let set_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
        write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
        assert_return_value!(
            d3d9::handle_set_sampler_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    let get_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, 0, u64::from(slot), out, 0);
        assert_return_value!(
            d3d9::handle_get_texture_stage_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetTextureStageState output");
        u32::from_le_bytes(bytes)
    };
    let get_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, 0, u64::from(slot), out, 0);
        assert_return_value!(
            d3d9::handle_get_sampler_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetSamplerState output");
        u32::from_le_bytes(bytes)
    };

    // The stage-0 defaults match the legacy resolve fallbacks.
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_MODULATE);
    assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_POINT);
    assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_WRAP);

    // Set TSS slots → typed fields.
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLOROP,
        crate::d3d9_render::D3DTOP_SELECTARG1,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLORARG1,
        crate::d3d9_render::D3DTA_TEXTURE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLORARG2,
        crate::d3d9_render::D3DTA_DIFFUSE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAOP,
        crate::d3d9_render::D3DTOP_MODULATE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAARG1,
        crate::d3d9_render::D3DTA_TEXTURE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAARG2,
        crate::d3d9_render::D3DTA_DIFFUSE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_TEXCOORDINDEX,
        3,
    );
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_SELECTARG1);
    assert_eq!(stage.color_arg1, crate::d3d9_render::D3DTA_TEXTURE);
    assert_eq!(stage.color_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
    assert_eq!(stage.alpha_op, crate::d3d9_render::D3DTOP_MODULATE);
    assert_eq!(stage.alpha_arg1, crate::d3d9_render::D3DTA_TEXTURE);
    assert_eq!(stage.alpha_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
    assert_eq!(stage.tex_coord_index, 3);

    // Set sampler slots → typed fields (D3DSAMP_* namespace).
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_MAGFILTER,
        crate::d3d9_render::D3DTEXF_LINEAR,
    );
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_ADDRESSU,
        crate::d3d9_render::D3DTADDRESS_CLAMP,
    );
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_ADDRESSV,
        crate::d3d9_render::D3DTADDRESS_CLAMP,
    );
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_LINEAR);
    assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_CLAMP);
    assert_eq!(stage.address_v, crate::d3d9_render::D3DTADDRESS_CLAMP);

    // Get* round-trips the typed slots.
    assert_eq!(
        get_tss(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
        crate::d3d9_render::D3DTOP_SELECTARG1
    );
    assert_eq!(
        get_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_COLORARG2
        ),
        crate::d3d9_render::D3DTA_DIFFUSE
    );
    assert_eq!(
        get_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_MAGFILTER
        ),
        crate::d3d9_render::D3DTEXF_LINEAR
    );
    assert_eq!(
        get_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSU
        ),
        crate::d3d9_render::D3DTADDRESS_CLAMP
    );

    // Unmodeled slots are preserved verbatim for the Get* round-trips.
    set_tss(&mut engine, &mut state, 99, 0xAB);
    set_samp(&mut engine, &mut state, 88, 0xCD);
    assert_eq!(get_tss(&mut engine, &mut state, 99), 0xAB);
    assert_eq!(get_samp(&mut engine, &mut state, 88), 0xCD);
    // The two namespaces stay separate despite the colliding constants:
    // TSS slot 1 is COLOROP (set to SELECTARG1 above), sampler slot 1 is
    // ADDRESSU (set to CLAMP above).
    assert_eq!(
        get_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSU
        ),
        crate::d3d9_render::D3DTOP_SELECTARG1
    );
    assert_eq!(
        get_samp(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
        crate::d3d9_render::D3DTADDRESS_CLAMP
    );
}

// --- LoadAcceleratorsW / TranslateAcceleratorW ---

/// Seed the main-module accelerator tables with notepad-like entries.
fn push_accel_tables(state: &mut WinApiState) {
    use wie_pe::resources::{AccelEntry, AccelTemplate};
    state.process.main_module_accelerators.push(AccelTemplate {
        id: 0x0100,
        lang: 0x0409,
        entries: vec![
            // FVIRTKEY|FCONTROL, VK_N → File New (0x0100).
            AccelEntry {
                flags: 0x09,
                key: 0x4E,
                command_id: 0x0100,
            },
            // FVIRTKEY|FSHIFT, VK_O → Save As (0x0103).
            AccelEntry {
                flags: 0x05,
                key: 0x4F,
                command_id: 0x0103,
            },
            // Plain char 'a' (no VIRTKEY) → 0x0111.
            AccelEntry {
                flags: 0x00,
                key: 0x61,
                command_id: 0x0111,
            },
        ],
    });
}

/// Run one user32 API through the full dispatch path (names.rs → dense id).
fn dispatch_user32(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let id = crate::resolve_winapi_id("user32.dll", name)
        .unwrap_or_else(|| panic!("{name} must resolve to a WinApiId"));
    let r = crate::dispatch_winapi_id(&mut HandlerContext::new(engine, default_env(), state), id)
        .expect("handler must dispatch");
    r.return_value
}

#[test]
fn test_load_accelerators_w_returns_cached_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    // MAKEINTRESOURCEW(0x100): hinst = image base, low word = table id.
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(first, 0, "known table id must return a nonzero HACCEL");
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let second = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_eq!(
        first, second,
        "the same table id must return the same HACCEL"
    );
}

#[test]
fn test_load_accelerators_w_unknown_table_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    // Table id 0x200 is not in the parsed set.
    write_regs(&mut engine, image_base, 0x200, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_eq!(handle, 0, "unknown table id must return NULL");
}

#[test]
fn test_translate_accelerator_w_posts_wm_command() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    // Load the table and note the cached HACCEL.
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(haccel, 0);

    // MSG struct at 0x4000: message = WM_KEYDOWN, wParam = VK_N (0x4E).
    let msg_ptr = 0x4000_u64;
    engine
        .mem_map(msg_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]); // alignment padding
    msg.extend_from_slice(&0x4E_u64.to_le_bytes());
    msg.extend_from_slice(&0_u64.to_le_bytes()); // lParam
    engine.mem_write(msg_ptr, &msg).expect("write MSG struct");
    // Ctrl is down (the table entry requires FCONTROL).
    state.window_state().keyboard_state.set(0x11, 0x80);

    write_regs(&mut engine, hwnd, haccel, msg_ptr, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 1, "matching VK + Ctrl must translate");

    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.window_handle.as_u64(), hwnd);
    assert_eq!(
        posted.word_parameter, 0x0100,
        "WM_COMMAND wParam = table id"
    );
    assert_eq!(posted.long_parameter, 0);
}

#[test]
fn test_translate_accelerator_w_no_match_posts_nothing() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");

    let msg_ptr = 0x4000_u64;
    engine
        .mem_map(msg_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // WM_KEYDOWN with VK_N but Ctrl NOT down: the FCONTROL entry must not match.
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x4E_u64.to_le_bytes());
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_ptr, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_ptr, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 0, "Ctrl-up must not translate");
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        0,
        "no WM_COMMAND may be posted"
    );
}

#[test]
fn test_translate_accelerator_w_plain_char() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");

    // A non-VIRTKEY entry matches a WM_CHAR whose wParam is the ANSI char.
    let msg_ptr = 0x4000_u64;
    engine
        .mem_map(msg_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_CHAR.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x61_u64.to_le_bytes()); // 'a'
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_ptr, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_ptr, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 1, "plain-char match must translate");
    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.word_parameter, 0x0111);
}

#[test]
fn test_translate_accelerator_a_mirrors_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsA");

    // Shift+O (0x05 = FVIRTKEY|FSHIFT = 0x01|0x04, VK_O) via the A variant.
    state.window_state().keyboard_state.set(0x10, 0x80); // VK_SHIFT
    let msg_ptr = 0x4000_u64;
    engine
        .mem_map(msg_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x4F_u64.to_le_bytes()); // VK_O
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_ptr, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_ptr, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorA");
    assert_eq!(translated, 1, "Shift+O must translate via the A variant");
    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.word_parameter, 0x0103);
}

#[test]
fn test_translate_accelerator_w_unknown_haccel_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_ptr = 0x4000_u64;
    engine
        .mem_map(msg_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    write_regs(
        &mut engine,
        0x6610_1000,
        0x0000_0000_6640_0005,
        msg_ptr,
        0,
        0,
    );
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 0, "an unallocated HACCEL must not translate");
}

#[test]
fn test_destroy_accelerator_table_frees_and_reloads_fresh() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(first, 0, "known table id must return a nonzero HACCEL");

    write_regs(&mut engine, first, 0, 0, 0, 0);
    let destroyed = dispatch_user32(&mut engine, &mut state, "DestroyAcceleratorTable");
    assert_eq!(destroyed, 1, "a loaded HACCEL must destroy successfully");

    // The freed (module, id) pair must allocate a fresh handle on reload.
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let reloaded = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(reloaded, 0, "reload of a freed table must still succeed");
    assert_ne!(
        reloaded, first,
        "a destroyed table must not return the stale HACCEL"
    );
}

#[test]
fn test_destroy_accelerator_table_unknown_handle_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // No table was ever loaded, so any handle is unknown.
    write_regs(&mut engine, 0x0000_0000_6640_00FF, 0, 0, 0, 0);
    let destroyed = dispatch_user32(&mut engine, &mut state, "DestroyAcceleratorTable");
    assert_eq!(destroyed, 0, "an unallocated HACCEL must not destroy");
}

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
    let logfont_ptr = 0x5000_u64;
    engine
        .mem_write(logfont_ptr, &(-16_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_ptr + 16, &(700_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_ptr + 20, &[1_u8])
        .expect("write LOGFONTA.lfItalic");
    engine
        .mem_write(logfont_ptr + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_ptr + 28, b"Courier New\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_ptr, 0, 0, 0, 0);
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
    let logfont_ptr = 0x5000_u64;
    engine
        .mem_write(logfont_ptr, &(-32_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_ptr + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_ptr + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_ptr + 28, b"Segoe UI\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(engine, logfont_ptr, 0, 0, 0, 0);
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
    let logfont_ptr = 0x5000_u64;
    engine
        .mem_write(logfont_ptr, &(-16_i32).to_le_bytes())
        .expect("write LOGFONTA.lfHeight");
    engine
        .mem_write(logfont_ptr + 16, &(400_i32).to_le_bytes())
        .expect("write LOGFONTA.lfWeight");
    engine
        .mem_write(logfont_ptr + 23, &[1_u8])
        .expect("write LOGFONTA.lfCharSet");
    engine
        .mem_write(logfont_ptr + 27, &[0x31_u8])
        .expect("write LOGFONTA.lfPitchAndFamily");
    engine
        .mem_write(logfont_ptr + 28, b"Lucida Console\0")
        .expect("write LOGFONTA.lfFaceName");
    write_regs(&mut engine, logfont_ptr, 0, 0, 0, 0);
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

// ─── EDIT subclass bridge (L1: CallWindowProc + GWLP_WNDPROC) ────────────
//
// notepad subclasses its EDIT: SetWindowLongPtrW(hEdit, GWLP_WNDPROC,
// EDIT_WndProc) replaces the control's proc with a guest callback that must
// see every message FIRST (it updates the title star and the status-bar
// Ln/Col), forwarding the rest through CallWindowProcW(hEdit, <original>,
// ...). The original value WIE hands back for a fresh control is 0 — the
// host's default-control-proc marker — and CallWindowProcW with that value
// must run the normal host control dispatch.

/// `GWLP_WNDPROC` (-4) as the zero-extended `u64` a Win64 `SetWindowLongPtrW`
/// index register carries it (MSVC emits `mov edx, -4`).
const GWLP_WNDPROC_RAW: u64 = 0xFFFF_FFFC;

/// A plausible guest code address for the subclass proc.
const GUEST_SUBCLASS: u64 = 0x0000_0000_1400_1000;

/// Install a guest subclass on `hwnd` through the real SetWindowLongPtrW
/// handler; returns the previous `GWLP_WNDPROC` value the handler reported.
fn subclass_edit(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64) -> u64 {
    write_regs(engine, hwnd, GWLP_WNDPROC_RAW, GUEST_SUBCLASS, 0, 0);
    user32::handle_set_window_long_ptr_w(&mut HandlerContext::new(engine, default_env(), state))
        .expect("set subclass")
        .return_value
}

/// Call `handle_call_window_proc_w` with `prev_wndproc` / `hwnd` / `message`
/// and a zero `lParam`; returns the handler's reported return value.
fn call_window_proc_default(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    prev_wndproc: u64,
    hwnd: u64,
    message: u32,
) -> u64 {
    write_regs(engine, prev_wndproc, hwnd, u64::from(message), 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .expect("write CallWindowProc lParam");
    user32::handle_call_window_proc_w(&mut HandlerContext::new(engine, default_env(), state))
        .expect("call window proc")
        .return_value
}

#[test]
fn test_edit_subclass_setwindowlongptr_returns_original_and_stores() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // First subclass: the previous GWLP_WNDPROC of a fresh control is WIE's
    // host-default marker (0) — that is what notepad's EDIT_WndProc stores
    // and passes back to CallWindowProcW.
    assert_eq!(
        subclass_edit(&mut engine, &mut state, edit),
        0,
        "the original host-default marker is returned"
    );

    // GetWindowLongPtrW now reports the subclass.
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get subclass")
    .return_value;
    assert_eq!(read_back, GUEST_SUBCLASS);

    // The record remembers the original (the marker CallWindowProcW treats
    // as "run the host default").
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record");
    assert_eq!(window.subclass_original_wndproc, 0);
}

#[test]
fn test_edit_subclass_wm_char_bridges_to_guest_subclass_first() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");
    subclass_edit(&mut engine, &mut state, edit);

    // A WM_CHAR to the subclassed EDIT must bridge to the guest subclass
    // FIRST (the notepad star path) — the host default must not run, so the
    // text stays untouched.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must bridge to the guest subclass");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
                    && request.window_handle == edit
                    && request.message == crate::user32::WM_CHAR
                    && request.word_parameter == u64::from(b'x')
                    && request.outer_return == crate::OuterReturn::Passthrough
        ),
        "WM_CHAR must bridge to the guest subclass, got {signal:?}"
    );
    assert_eq!(
        control_text(&state, edit),
        "hello",
        "the host default dispatch must not run while subclassed"
    );
}

#[test]
fn test_edit_subclass_wm_keydown_bridges_to_guest_subclass_first() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");
    subclass_edit(&mut engine, &mut state, edit);

    // Arrow-key navigation (the status-bar Ln/Col path) also reaches the
    // subclass before the host sees it.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_RIGHT,
        0,
    );
    let error = result.expect_err("WM_KEYDOWN must bridge to the guest subclass");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
                    && request.message == crate::user32::WM_KEYDOWN
                    && request.word_parameter == crate::user32::VK_RIGHT
        ),
        "WM_KEYDOWN must bridge to the guest subclass, got {signal:?}"
    );
    let caret = control_ui(&state, edit);
    assert_eq!(caret.caret, 0, "the host caret move must not run");
}

#[test]
fn test_edit_subclass_callwindowproc_runs_host_default_dispatch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");
    let font = state
        .gdi_state()
        .alloc_font("Segoe UI".to_owned(), -16, 400, false, 0);

    // Store a font through the real control dispatch (pre-subclass).
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

    subclass_edit(&mut engine, &mut state, edit);

    // The subclass's CallWindowProcW(hEdit, <original marker>, WM_GETFONT)
    // runs the HOST default control dispatch and returns its LRESULT.
    assert_eq!(
        call_window_proc_default(&mut engine, &mut state, 0, edit, crate::user32::WM_GETFONT),
        font.as_u64(),
        "CallWindowProc with the original marker must run the host default"
    );

    // EM_GETLINECOUNT through the same bridge: "ab\ncd" is 2 lines.
    assert_eq!(
        call_window_proc_default(
            &mut engine,
            &mut state,
            0,
            edit,
            crate::user32::EM_GETLINECOUNT,
        ),
        2,
        "CallWindowProc must surface the host default LRESULT"
    );

    // A foreign (non-marker, non-zero) proc is bridged to the guest — what
    // real Windows does (call that proc) — not run as the default.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("subclassed control still bridges");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
        ),
        "the stored subclass is the bridge target, got {signal:?}"
    );
}

#[test]
fn test_edit_unsubclass_restores_host_dispatch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");

    assert_eq!(subclass_edit(&mut engine, &mut state, edit), 0);
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    result
        .expect_err("subclassed WM_CHAR must bridge")
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");

    // notepad's WM_DESTROY / DoCreateEditWindow "restore" passes the saved
    // original back: SetWindowLongPtrW(hEdit, GWLP_WNDPROC, 0).
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let previous = user32::handle_set_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("restore subclass")
    .return_value;
    assert_eq!(previous, GUEST_SUBCLASS, "restoring returns the subclass");

    // With GWLP_WNDPROC back to null the control dispatch runs host-side
    // again: typing mutates the text and the change is delivered to the
    // guest-WndProc parent as EN_CHANGE — NO subclass bridge.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must reach the host dispatch");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == 0x7000_0000
                    && request.message == crate::user32::WM_COMMAND
        ),
        "the restored edit delivers EN_CHANGE to its parent, got {signal:?}"
    );
    assert_eq!(control_text(&state, edit), "xhello");

    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get after restore")
    .return_value;
    assert_eq!(read_back, 0, "GWLP_WNDPROC reads 0 after un-subclassing");
}

#[test]
fn test_edit_without_subclass_behaves_exactly_as_before() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");

    // No subclass bridge: WM_CHAR mutates the text host-side and the change
    // reaches the guest-WndProc parent as EN_CHANGE (the long-standing
    // behavior — the signal targets the parent, never the control itself).
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must reach the host dispatch");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == 0x7000_0000
                    && request.message == crate::user32::WM_COMMAND
        ),
        "an un-subclassed edit delivers EN_CHANGE to its parent, got {signal:?}"
    );
    assert_eq!(control_text(&state, edit), "xhello");

    // GWLP_WNDPROC reads 0 (nothing stored).
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get wndproc")
    .return_value;
    assert_eq!(read_back, 0);

    // CallWindowProcW(hEdit, 0, WM_GETFONT) on an un-subclassed control is
    // the conservative default path: it runs the host dispatch (0 font, no
    // crash) rather than invoking anything.
    assert_eq!(
        call_window_proc_default(&mut engine, &mut state, 0, edit, crate::user32::WM_GETFONT),
        0,
        "marker CallWindowProc on an un-subclassed control stays host-side"
    );
}

// ── gdi32 lane: TEXTMETRIC byte layouts (zerocopy writes) ─────────────

/// Create a memory DC and return its HDC through the real handler.
fn create_memory_dc(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    let r = crate::gdi32::handle_create_compatible_dc(&mut HandlerContext::new(
        engine,
        test_environment(),
        state,
    ))
    .expect("CreateCompatibleDC must succeed");
    r.return_value
}

/// Read `len` guest bytes at `addr` into a Vec (avoids slice-typed buffers).
fn read_guest_bytes(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    engine.mem_read(addr, &mut bytes).expect("read guest bytes");
    bytes
}

/// GetTextMetricsA writes the TEXTMETRICA layout through the typed view:
/// LONGs @0..43, BYTE char fields @44..52, 3 trailing pad bytes (56 total).
#[test]
fn test_get_text_metrics_a_writes_textmetrica_byte_layout() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hdc = create_memory_dc(&mut engine, &mut state);
    let metrics_ptr = 0x4000_u64;
    write_regs(&mut engine, hdc, metrics_ptr, 0, 0, 0);
    let r = crate::gdi32::handle_get_text_metrics_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetTextMetricsA must dispatch");
    assert_eq!(r.return_value, 1);

    let bytes = read_guest_bytes(&mut engine, metrics_ptr, 56);
    let height = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let weight = i32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    assert!(height > 0, "resolved font height is positive");
    assert_eq!(weight, 400, "regular weight default");
    // A char fields are single BYTEs at 44..47; flags follow at 48..52.
    assert_eq!(&bytes[44..48], &[0, 0, 0, 0], "tmFirstChar..tmBreakChar");
    assert_eq!(bytes[48], 0, "tmItalic @48");
    assert_eq!(bytes[51], 0x01, "tmPitchAndFamily @51");
    assert_eq!(bytes[52], 0, "tmCharSet @52");
    assert_eq!(&bytes[53..56], &[0, 0, 0], "trailing pad @53..55 zeroed");
}

/// GetTextMetricsW writes the TEXTMETRICW layout: WCHAR char fields @44..51,
/// flags @52..56, 3 trailing pad bytes (60 total) — the A/W offset shift.
#[test]
fn test_get_text_metrics_w_writes_textmetricw_byte_layout() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hdc = create_memory_dc(&mut engine, &mut state);
    let metrics_ptr = 0x4000_u64;
    write_regs(&mut engine, hdc, metrics_ptr, 0, 0, 0);
    let r = crate::gdi32::handle_get_text_metrics_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetTextMetricsW must dispatch");
    assert_eq!(r.return_value, 1);

    let bytes = read_guest_bytes(&mut engine, metrics_ptr, 60);
    let height = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let weight = i32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    assert!(height > 0, "resolved font height is positive");
    assert_eq!(weight, 400, "regular weight default");
    // W char fields are 2-byte WCHARs at 44..51 (vs BYTEs at 44..47 in A).
    assert_eq!(&bytes[44..52], &[0; 8], "tmFirstChar..tmBreakChar WCHARs");
    assert_eq!(bytes[52], 0, "tmItalic @52");
    assert_eq!(bytes[55], 0x01, "tmPitchAndFamily @55");
    assert_eq!(bytes[56], 0, "tmCharSet @56");
    assert_eq!(&bytes[57..60], &[0, 0, 0], "trailing pad @57..59 zeroed");
}
