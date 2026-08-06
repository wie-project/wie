//! Window management tests: GetWindowPlacement / SetWindowPlacement geometry, visibility, and erase/paint cycles, plus the wake-on-destroy host-sync contract.
use super::*;

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
