//! Unit tests for WinAPI handler dispatch and the state types.
//!
//! Split by theme into the sibling modules below (moved verbatim from the old
//! single-file `tests.rs`; the readability wave comes later). The shared
//! scaffolding and control fixtures stay in this module so every themed file
//! just needs `use super::*;`.
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

// ── Shared control fixtures (used by several themed files) ───────────
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

// ── Shared window / message helpers ─────────────────────────────────
/// Read a guest i32 at `addr` (test helper mirroring `read_guest_i32`).
fn read_test_i32(engine: &mut IcedCpu, addr: u64) -> i32 {
    let mut bytes = [0_u8; 4];
    engine.mem_read(addr, &mut bytes).expect("read guest i32");
    i32::from_le_bytes(bytes)
}

/// A client point packed into an lParam (x = low word, y = high word).
fn mouse_lparam(x: u16, y: u16) -> u64 {
    u64::from(x) | (u64::from(y) << 16)
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

/// Run one user32 API through the full dispatch path (names.rs → dense id).
fn dispatch_user32(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let id = crate::resolve_winapi_id("user32.dll", name)
        .unwrap_or_else(|| panic!("{name} must resolve to a WinApiId"));
    let r = crate::dispatch_winapi_id(&mut HandlerContext::new(engine, default_env(), state), id)
        .expect("handler must dispatch");
    r.return_value
}

// ── Shared EDIT scroll helper ──────────────────────────────────────
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

// ── Shared D3D9 test constants ────────────────────────────────────
/// D3DCLEAR_TARGET (d3d9.rs keeps these private).
const D3DCLEAR_TARGET: u32 = 0x0000_0001;
/// D3DTS_WORLD.
const D3DTS_WORLD: u32 = 256;
/// D3DPT_TRIANGLELIST.
const D3DPT_TRIANGLELIST: u32 = 4;

mod accelerators;
mod buttons;
mod common_dialogs;
mod d3d9_state_tests;
mod d3d9_tests;
mod d3d9_texture_tests;
mod edit_core;
mod edit_layout;
mod edit_model;
mod edit_mouse;
mod edit_paint;
mod edit_rnotepad;
mod edit_scroll;
mod edit_undo;
mod fake_handles;
mod gdi;
mod kernel32_stub_tests;
mod kernel32_tests;
mod listbox;
mod menu;
mod message_box;
mod modal_frame;
mod ole_auto;
mod registry;
mod repaint;
mod seh_dispatch;
mod setfont;
mod shell;
mod status_bar;
mod subclass;
mod user32_tests;
mod version;
mod window_placement;
mod winmm_tests;
