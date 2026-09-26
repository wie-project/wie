//! Phase-2 stub-wave tests: the DOOM Retro / SDL2 boot-surface handlers added
//! in the batch (critical-section Ex/Try, mutex, SList, version, power status,
//! trivial kernel32 returns, the user32 geometry/clipboard/keyboard stubs and
//! the real math helpers IntersectRect / PtInRect).
use super::*;
use crate::user32::write_guest_u32;

/// Dispatch one kernel32 API through the extra-dispatch table.
fn dispatch_k32(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    kernel32::dispatch_kernel32_extra(&mut ctx, name)
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

/// Dispatch one user32 API through the extra-dispatch table.
fn dispatch_u32(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    user32::dispatch_user32_extra(&mut ctx, name)
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

#[test]
fn test_initialize_critical_section_ex_writes_spin_count() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cs = 0x3000_u64;
    write_regs(&mut engine, cs, 0x1000, 0, 0, 0);
    dispatch_k32(&mut engine, &mut state, "InitializeCriticalSectionEx");
    let mut spin = [0_u8; 8];
    engine.mem_read(cs + 0x20, &mut spin).expect("spin count");
    assert_eq!(u64::from_le_bytes(spin), 0x1000);
    let mut lock = [0_u8; 4];
    engine.mem_read(cs + 8, &mut lock).expect("lock count");
    assert_eq!(u32::from_le_bytes(lock), u32::MAX, "unlocked = -1");
}

#[test]
fn test_try_enter_critical_section_acquires_when_free() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cs = 0x3000_u64;
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    dispatch_k32(&mut engine, &mut state, "InitializeCriticalSectionEx");
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "TryEnterCriticalSection"),
        1
    );
    // Owned by the current thread now.
    let mut owner = [0_u8; 8];
    engine.mem_read(cs + 16, &mut owner).expect("owner");
    assert_eq!(u64::from_le_bytes(owner), u64::from(PRIMARY_THREAD_ID));
}

#[test]
fn test_try_enter_critical_section_fails_when_owned_by_other() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cs = 0x3000_u64;
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    dispatch_k32(&mut engine, &mut state, "InitializeCriticalSectionEx");
    // Simulate another thread owning it: write OwningThread = 0x999.
    engine
        .mem_write(cs + 16, &0x999_u64.to_le_bytes())
        .expect("owner");
    write_regs(&mut engine, cs, 0, 0, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "TryEnterCriticalSection"),
        0,
        "contended lock must not be acquired"
    );
}

#[test]
fn test_create_mutex_a_and_release_mutex_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let handle = dispatch_k32(&mut engine, &mut state, "CreateMutexA");
    assert_ne!(handle, 0, "mutex handle must be non-NULL");
    // A freshly created mutex is signaled (waitable).
    let sem = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Semaphore(sem)) => sem.clone(),
        _ => {
            panic!("mutex must be a semaphore-backed object");
        }
    };
    let wait = crate::wait_multiple(&[crate::WaitTarget::Semaphore(sem)], true, 0);
    assert_eq!(wait, crate::WAIT_OBJECT_0);
    // ReleaseMutex succeeds and re-signals.
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(dispatch_k32(&mut engine, &mut state, "ReleaseMutex"), 1);
    assert_eq!(dispatch_k32(&mut engine, &mut state, "ReleaseMutex"), 1);
}

#[test]
fn test_slist_head_init_and_flush() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let head = 0x3000_u64;
    // Seed a first-element pointer (tagged) + depth.
    engine
        .mem_write(head, &(0x1234_u64 | 0x7).to_le_bytes())
        .expect("head");
    write_regs(&mut engine, head, 0, 0, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "InterlockedFlushSList"),
        0x1230,
        "flush returns the masked first element"
    );
    let mut cleared = [0_u8; 16];
    engine.mem_read(head, &mut cleared).expect("cleared head");
    assert_eq!(cleared, [0_u8; 16], "header reset to empty");

    write_regs(&mut engine, head, 0, 0, 0, 0);
    dispatch_k32(&mut engine, &mut state, "InitializeSListHead");
    engine.mem_read(head, &mut cleared).expect("zeroed head");
    assert_eq!(cleared, [0_u8; 16]);
}

#[test]
fn test_get_process_id_pseudohandle_returns_current_pid() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, u64::MAX, 0, 0, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "GetProcessId"),
        crate::kernel32::FAKE_CURRENT_PROCESS_ID
    );
}

#[test]
fn test_trivial_kernel32_constants() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(dispatch_k32(&mut engine, &mut state, "SetStdHandle"), 1);
    assert_eq!(dispatch_k32(&mut engine, &mut state, "CancelIo"), 1);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "SetThreadExecutionState"),
        0
    );
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "SetThreadPriority"),
        1
    );
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "UnhandledExceptionFilter"),
        0
    );
}

#[test]
fn test_ver_set_condition_mask_packs_condition() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // VerSetConditionMask(mask=0, TypeBitMask=1 /*VER_MAJORVERSION*/, Condition=3 /*VER_GREATER_EQUAL*/)
    write_regs(&mut engine, 0, 1, 3, 0, 0);
    let mask = dispatch_k32(&mut engine, &mut state, "VerSetConditionMask");
    assert_eq!(mask & 0b11, 0b11, "condition 3 packed into type-1 pair");
    assert_eq!(mask >> 2, 0, "no other type bits touched");
}

#[test]
fn test_verify_version_info_w_matches_guest_os() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let info = 0x3000_u64;
    // OSVERSIONINFOEXW: size=284, major=10, minor=0, build=19045, platform=2.
    let mut b = [0_u8; 0x14];
    b[0..4].copy_from_slice(&284_u32.to_le_bytes());
    b[4..8].copy_from_slice(&10_u32.to_le_bytes());
    b[8..12].copy_from_slice(&0_u32.to_le_bytes());
    b[12..16].copy_from_slice(&19045_u32.to_le_bytes());
    b[16..20].copy_from_slice(&2_u32.to_le_bytes());
    engine.mem_write(info, &b).expect("write OSVERSIONINFOEXW");
    // Condition mask: VER_MAJORVERSION(1)=VER_GREATER_EQUAL(3) → bits 0-1 = 3;
    // VER_MINORVERSION(2)=VER_GREATER_EQUAL(3) → bits 2-3 = 3.
    write_regs(&mut engine, info, 0x3, 0b1111, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "VerifyVersionInfoW"),
        1,
        "guest OS 10.0.19045 satisfies >= 10.0"
    );
    // A version requirement of 11.0 must fail.
    b[4..8].copy_from_slice(&11_u32.to_le_bytes());
    engine.mem_write(info, &b).expect("rewrite version");
    write_regs(&mut engine, info, 0x3, 0b1111, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "VerifyVersionInfoW"),
        0
    );
}

#[test]
fn test_get_system_power_status_on_ac_no_battery() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let status = 0x3000_u64;
    write_regs(&mut engine, status, 0, 0, 0, 0);
    assert_eq!(
        dispatch_k32(&mut engine, &mut state, "GetSystemPowerStatus"),
        1
    );
    let mut b = [0_u8; 12];
    engine.mem_read(status, &mut b).expect("power status");
    assert_eq!(b[0], 1, "AC line");
    assert_eq!(b[1], 0x80, "no battery");
}

// ── user32 batch ───────────────────────────────────────────────────────────

#[test]
fn test_intersect_rect_math() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let out = 0x3000_u64;
    let r1 = 0x3100_u64;
    let r2 = 0x3200_u64;
    // r1 = (0,0,100,100), r2 = (50,50,150,150)
    let mut b = [0_u8; 16];
    for (i, v) in [0_i32, 0, 100, 100].iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    engine.mem_write(r1, &b).expect("r1");
    for (i, v) in [50_i32, 50, 150, 150].iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    engine.mem_write(r2, &b).expect("r2");
    write_regs(&mut engine, out, r1, r2, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "IntersectRect"),
        1,
        "overlapping rects intersect"
    );
    let mut res = [0_i32; 4];
    engine.mem_read(out, &mut b).expect("result");
    for (i, chunk) in b.chunks(4).enumerate() {
        res[i] = i32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
    }
    assert_eq!(res, [50, 50, 100, 100]);
    // Disjoint rects: FALSE, result untouched.
    engine.mem_write(r1, &b).expect("r1 kept");
    for (i, v) in [200_i32, 200, 300, 300].iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    engine.mem_write(r2, &b).expect("r2 moved");
    write_regs(&mut engine, out, r1, r2, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "IntersectRect"),
        0,
        "disjoint rects do not intersect"
    );
}

#[test]
fn test_pt_in_rect_math() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let rect = 0x3000_u64;
    let mut b = [0_u8; 16];
    for (i, v) in [10_i32, 20, 110, 120].iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    engine.mem_write(rect, &b).expect("rect");
    write_regs(&mut engine, rect, 50, 70, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "PtInRect"), 1);
    write_regs(&mut engine, rect, 5, 70, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "PtInRect"),
        0,
        "left of rect"
    );
    write_regs(&mut engine, rect, 110, 70, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "PtInRect"),
        0,
        "right edge exclusive"
    );
}

#[test]
fn test_trivial_user32_constants() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "AttachThreadInput"),
        1
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetDoubleClickTime"),
        500
    );
    assert_eq!(dispatch_u32(&mut engine, &mut state, "GetMessageTime"), 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetMessageExtraInfo"),
        0
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetKeyboardLayout"),
        0
    );
    assert_eq!(dispatch_u32(&mut engine, &mut state, "SetCursorPos"), 1);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "WaitForInputIdle"), 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "RegisterHotKey"), 1);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "UnregisterHotKey"), 1);
    // `uiNumDevices == 0` is a no-op success on Windows: registering nothing
    // always succeeds, so the pre-synthesis "always FALSE" pin is gone. The
    // "no devices, fall back to WM_MOUSE*" behaviour it encoded no longer
    // applies — see `test_register_raw_input_devices_round_trips`.
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "RegisterRawInputDevices"),
        1,
        "uiNumDevices == 0 is a successful no-op"
    );
    assert_eq!(dispatch_u32(&mut engine, &mut state, "keybd_event"), 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "ToUnicode"), 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "SetLayeredWindowAttributes"),
        1
    );
    assert_eq!(dispatch_u32(&mut engine, &mut state, "SetWindowRgn"), 1);
}

#[test]
fn test_window_props_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = 0x1111_u64;
    let name_va = 0x5000_u64;
    write_guest_ansi(&mut engine, name_va, "SDL_WindowData");
    // GetProp before SetProp → NULL.
    write_regs(&mut engine, hwnd, name_va, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "GetPropW"), 0);
    // SetProp stores the value.
    write_regs(&mut engine, hwnd, name_va, 0xABCD, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "SetPropW"), 1);
    write_regs(&mut engine, hwnd, name_va, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "GetPropW"), 0xABCD);
    // RemoveProp returns the value and clears it.
    write_regs(&mut engine, hwnd, name_va, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "RemovePropW"), 0xABCD);
    write_regs(&mut engine, hwnd, name_va, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "GetPropW"), 0);
}
/// Serializes the raw-input tests below.
///
/// `raw_input_state()` is a process-wide `static`, and every one of these tests
/// `clear()`s it as its first act. nextest runs test threads in parallel, so
/// without this guard one test can wipe another's pending records between its
/// own enqueue and its assertion, which surfaces as an intermittent
/// "expected 96, got 0" rather than as an obvious race.
static RAW_INPUT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn raw_input_test_guard() -> std::sync::MutexGuard<'static, ()> {
    RAW_INPUT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// `GetRawInputDeviceList` now reports the two synthesized devices (keyboard +
/// mouse) instead of an empty list.
///
/// This test was `test_raw_input_device_list_is_empty`: it pinned the
/// pre-synthesis stub that reported zero devices, which made every guest that
/// probes RawInput (SDL first) silently degrade to the `WM_MOUSE*` bridge. The
/// empty-list pin is obsolete, so the assertion now pins the real contract —
/// including the Windows two-call protocol where a NULL list only reports the
/// count.
#[test]
fn test_raw_input_device_list_reports_synthesized_devices() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let count_va = 0x3000_u64;
    let list_va = 0x4000_u64;

    // First call: NULL list → the count only. cbSize must be valid even here.
    write_guest_u32(&mut engine, count_va, 0).expect("write guest u32");
    write_regs(
        &mut engine,
        0,
        count_va,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_LIST_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceList"),
        0,
        "NULL list reports the count instead of the entries"
    );
    assert_eq!(read_guest_u32_test(&mut engine, count_va), 2);

    // Second call: fill the list.
    write_guest_u32(&mut engine, count_va, 2).expect("write guest u32");
    write_regs(
        &mut engine,
        list_va,
        count_va,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_LIST_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceList"),
        2,
        "two synthesized devices"
    );
    assert_eq!(read_guest_u32_test(&mut engine, count_va), 2);

    // RAWINPUTDEVICELIST: hDevice @0, dwType @8 (mouse first, then keyboard).
    let mut entry = [0_u8; 16];
    engine
        .mem_read(list_va, &mut entry)
        .expect("read first device entry");
    assert_eq!(
        u64::from_le_bytes(entry[0..8].try_into().unwrap_or([0; 8])),
        user32::raw_input::SYNTHESIZED_MOUSE_DEVICE,
    );
    assert_eq!(
        u32::from_le_bytes(entry[8..12].try_into().unwrap_or([0; 4])),
        0,
        "dwType RIM_TYPEMOUSE"
    );
    engine
        .mem_read(list_va + 16, &mut entry)
        .expect("read second device entry");
    assert_eq!(
        u64::from_le_bytes(entry[0..8].try_into().unwrap_or([0; 8])),
        user32::raw_input::SYNTHESIZED_KEYBOARD_DEVICE,
    );
    assert_eq!(
        u32::from_le_bytes(entry[8..12].try_into().unwrap_or([0; 4])),
        1,
        "dwType RIM_TYPEKEYBOARD"
    );
}

/// A full `RegisterRawInputDevices` → `GetRegisteredRawInputDevices`
/// round-trip, plus the `RIDEV_REMOVE` unregister path.
#[test]
fn test_register_raw_input_devices_round_trips() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let devices_va = 0x4000_u64;
    let out_va = 0x5000_u64;
    let count_va = 0x3000_u64;
    let hwnd = 0x1234_u64;

    // Register keyboard + mouse for `hwnd`.
    write_raw_input_devices(
        &mut engine,
        devices_va,
        &[
            (1, 6, 0, hwnd),
            (1, 2, user32::raw_input::RIDEV_EXCLUDE, hwnd),
        ],
    );
    write_regs(
        &mut engine,
        devices_va,
        2,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "RegisterRawInputDevices"),
        1
    );

    // A NULL puiNumDevices is a caller error.
    write_regs(
        &mut engine,
        out_va,
        0,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRegisteredRawInputDevices"),
        0
    );

    // A wrong cbSize is a caller error too.
    write_guest_u32(&mut engine, count_va, 8).expect("write guest u32");
    write_regs(&mut engine, out_va, count_va, 8, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRegisteredRawInputDevices"),
        0,
        "cbSize must be sizeof(RAWINPUTDEVICE)"
    );

    // Capacity 1 < 2 registrations: report the needed count, copy nothing.
    write_guest_u32(&mut engine, count_va, 1).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        count_va,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRegisteredRawInputDevices"),
        0,
        "short buffer reports the needed count"
    );
    assert_eq!(read_guest_u32_test(&mut engine, count_va), 2);

    // Enough capacity: both registrations come back, flags preserved.
    write_guest_u32(&mut engine, count_va, 2).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        count_va,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRegisteredRawInputDevices"),
        2
    );
    assert_eq!(read_guest_u32_test(&mut engine, count_va), 2);
    assert_eq!(read_guest_u16_test(&mut engine, out_va), 1, "usUsagePage");
    assert_eq!(read_guest_u16_test(&mut engine, out_va + 2), 6, "usUsage");
    let mut flags = [0_u8; 4];
    engine
        .mem_read(out_va + 4, &mut flags)
        .expect("read dwFlags");
    assert_eq!(u32::from_le_bytes(flags), 0);
    let mut target = [0_u8; 8];
    engine
        .mem_read(out_va + 8, &mut target)
        .expect("read hwndTarget");
    assert_eq!(u64::from_le_bytes(target), hwnd);
    assert_eq!(read_guest_u16_test(&mut engine, out_va + 16), 1, "2nd page");
    assert_eq!(
        read_guest_u16_test(&mut engine, out_va + 18),
        2,
        "2nd usage"
    );
    let mut flags2 = [0_u8; 4];
    engine
        .mem_read(out_va + 16 + 4, &mut flags2)
        .expect("read 2nd dwFlags");
    assert_eq!(
        u32::from_le_bytes(flags2),
        user32::raw_input::RIDEV_EXCLUDE,
        "RIDEV_EXCLUDE survives the round trip"
    );

    // RIDEV_REMOVE unregisters the mouse class only.
    write_raw_input_devices(
        &mut engine,
        devices_va,
        &[(1, 2, user32::raw_input::RIDEV_REMOVE, hwnd)],
    );
    write_regs(
        &mut engine,
        devices_va,
        1,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "RegisterRawInputDevices"),
        1
    );
    write_guest_u32(&mut engine, count_va, 8).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        count_va,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRegisteredRawInputDevices"),
        1,
        "only the keyboard registration survives RIDEV_REMOVE"
    );
    assert_eq!(read_guest_u16_test(&mut engine, out_va + 2), 6, "keyboard");
}

/// `RIDEV_PAGEONLY` makes a registration a page-level mask that covers every
/// usage on the page — the behaviour `is_registered` reports to the later
/// `WM_INPUT` lane.
#[test]
fn test_pageonly_registration_covers_every_usage_on_the_page() {
    let _guard = raw_input_test_guard();
    let raw = user32::raw_input::raw_input_state();
    raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let hwnd = 0x99_u64;
    raw.lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(user32::raw_input::RawInputDevice {
            usage_page: 1,
            usage: 0xFFFF,
            flags: user32::raw_input::RIDEV_PAGEONLY,
            target_window: hwnd,
        });
    let raw = user32::raw_input::raw_input_state();
    // Scope the guard: `guard_is_excluded` locks `raw` itself, and
    // std::sync::Mutex is not reentrant, so holding `guard` across that call
    // self-deadlocks.
    {
        let guard = raw.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.is_registered(hwnd, 1, 6), "keyboard on the page");
        assert!(guard.is_registered(hwnd, 1, 2), "mouse on the page");
        assert!(!guard.is_registered(hwnd, 2, 6), "another page");
    }
    // The pageonly mask must not leak into a real device class.
    assert!(
        !guard_is_excluded(raw, hwnd, 1, 6),
        "RIDEV_PAGEONLY alone is not RIDEV_EXCLUDE"
    );
}

/// `RIDEV_EXCLUDE` marks the class so the later lane suppresses the legacy
/// `WM_MOUSE*` / `WM_KEY*` messages for it.
#[test]
fn test_exclude_registration_suppresses_legacy_messages() {
    let _guard = raw_input_test_guard();
    let raw = user32::raw_input::raw_input_state();
    raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let hwnd = 0x77_u64;
    raw.lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(user32::raw_input::RawInputDevice {
            usage_page: 1,
            usage: 2,
            flags: user32::raw_input::RIDEV_EXCLUDE,
            target_window: hwnd,
        });
    let raw = user32::raw_input::raw_input_state();
    assert!(guard_is_excluded(raw, hwnd, 1, 2));
    assert!(!guard_is_excluded(raw, hwnd, 1, 6), "only the mouse class");
}

/// `GetRawInputData` sizes first, refuses an undersized buffer, then copies —
/// the three-call protocol a guest uses to walk a `WM_INPUT` lParam.
#[test]
fn test_get_raw_input_data_sizes_then_copies() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    let record_va = 0x6000_u64;
    write_regs(&mut engine, record_va, 0, 0, 0, 0);
    let r = dispatch_def_raw_input_buffer(&mut engine, &mut state, 0, &size_va);
    assert_eq!(r, 0, "no records yet");

    // Synthesize one keyboard record, then place the packed bytes in guest
    // memory exactly as the WM_INPUT lane will — hRawInput is that address.
    user32::raw_input::enqueue_raw_keyboard(0x1234, 0x1E, 0, 0x41, 0x0100);
    let drained = user32::raw_input::drain_raw_input_for_window(0x1234);
    let record = drained.first().expect("the synthesized record");
    engine
        .mem_write(record_va, &record.bytes)
        .expect("write the RAWINPUT behind hRawInput");

    // Size query: *pcbSize == 0 and pData == NULL reports the record size.
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(
        &mut engine,
        record_va,
        u64::from(user32::raw_input::RID_INPUT),
        0,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        0,
        "NULL pData only reports the size"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE,
        "24 header + 16 keyboard"
    );

    // Undersized buffer → ERROR_INVALID_PARAMETER and (UINT)-1.
    write_guest_u32(&mut engine, size_va, 8).expect("write guest u32");
    write_regs(
        &mut engine,
        record_va,
        u64::from(user32::raw_input::RID_INPUT),
        out_va,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        u64::from(u32::MAX),
        "undersized buffer is rejected"
    );
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");

    // Sized copy.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        record_va,
        u64::from(user32::raw_input::RID_INPUT),
        out_va,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        u64::from(crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE)
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE,
        "the size slot reports what was copied"
    );
    let mut out = [0_u8; 40];
    engine.mem_read(out_va, &mut out).expect("read RAWINPUT");
    assert_eq!(
        u32::from_le_bytes(out[0..4].try_into().unwrap_or([0; 4])),
        1
    );
    assert_eq!(
        u32::from_le_bytes(out[4..8].try_into().unwrap_or([0; 4])),
        40
    );
    assert_eq!(
        u16::from_le_bytes(out[24..26].try_into().unwrap_or([0; 2])),
        0x1E
    );
    assert_eq!(
        u16::from_le_bytes(out[30..32].try_into().unwrap_or([0; 2])),
        0x41
    );
}

/// `GetRawInputBuffer` packs records back to back — no per-record alignment —
/// sets each `header.dwSize` to that record's own packed size, and reports a
/// buffer size equal to the sum of the packed sizes.
#[test]
fn test_get_raw_input_buffer_packs_records_back_to_back() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let raw = user32::raw_input::raw_input_state();
    raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    raw.lock().unwrap_or_else(|e| e.into_inner()).enqueue_mouse(
        0x1234,
        user32::raw_input::RawMouse {
            mouse_flags: crate::guest_layout::MOUSE_MOVE_RELATIVE,
            _flags_pad: [0; 2],
            button_flags: 0,
            button_data: 0,
            raw_buttons: 0,
            last_x: 5,
            last_y: -5,
            extra_information: 0,
        },
    );
    raw.lock()
        .unwrap_or_else(|e| e.into_inner())
        .enqueue_keyboard(
            0x1234,
            user32::raw_input::RawKeyboard {
                make_code: 0x1E,
                flags: crate::guest_layout::RI_KEY_MAKE,
                reserved: 0,
                vkey: 0x41,
                message: 0x0100,
                extra_information: 0,
            },
        );

    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    // Needed size = 48 (mouse) + 40 (keyboard) = 88, no padding.
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(&mut engine, 0, size_va, 0, 0, 0);
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        0,
        "NULL pData reports the size"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        u64::from(
            crate::guest_layout::RAW_INPUT_MOUSE_RECORD_SIZE
                + crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE
        ) as u32
    );

    // Under-sized buffer copies nothing.
    write_guest_u32(&mut engine, size_va, 16).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        size_va,
        u64::from(user32::raw_input::RAW_INPUT_HEADER_SIZE),
        0,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        0,
        "under-sized buffer copies nothing"
    );

    // Sized copy: two records, the second starting exactly at +48.
    write_guest_u32(&mut engine, size_va, 128).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        size_va,
        u64::from(user32::raw_input::RAW_INPUT_HEADER_SIZE),
        0,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        2,
        "two records returned"
    );
    let mut out = [0_u8; 88];
    engine
        .mem_read(out_va, &mut out)
        .expect("read packed buffer");
    let word = |r: &[u8], at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap_or([0; 4]));
    assert_eq!(word(&out, 0), 0, "record 0 dwType = mouse");
    assert_eq!(word(&out, 4), 48, "record 0 dwSize = 48");
    assert_eq!(word(&out, 48), 1, "record 1 dwType = keyboard");
    assert_eq!(word(&out, 52), 40, "record 1 dwSize = 40");
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        88,
        "the size slot is the sum of the packed record sizes"
    );
}

/// `GetRawInputBuffer` rejects a `cbSizeHeader` that is not
/// `sizeof(RAWINPUTHEADER)`, exactly like Windows.
#[test]
fn test_get_raw_input_buffer_rejects_a_bad_header_size() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let size_va = 0x3000_u64;
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(&mut engine, 0, size_va, 0, 0, 0);
    write_stack_arg5(&mut engine, 12);
    assert_eq!(dispatch_get_raw_input_buffer(&mut engine, &mut state), 0);
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

/// `GetRawInputDeviceInfo` reports the device path length first, then writes
/// the string, and refuses an undersized buffer with
/// `ERROR_INSUFFICIENT_BUFFER`.
#[test]
fn test_get_raw_input_device_info_reports_size_then_writes() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    let device = user32::raw_input::SYNTHESIZED_MOUSE_DEVICE;

    // Size query.
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(user32::raw_input::RIDI_DEVICENAME),
        0,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoA"),
        0
    );
    let needed = read_guest_u32_test(&mut engine, size_va);
    assert!(needed > 1, "the path is not empty");

    // Under-sized buffer.
    write_guest_u32(&mut engine, size_va, 1).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(user32::raw_input::RIDI_DEVICENAME),
        out_va,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoA"),
        0
    );
    assert_eq!(state.process.last_error, 122, "ERROR_INSUFFICIENT_BUFFER");

    // Sized copy: return value excludes the NUL, the slot includes it.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(user32::raw_input::RIDI_DEVICENAME),
        out_va,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoA"),
        u64::from(needed - 1),
        "characters copied, NUL excluded"
    );
    assert_eq!(read_guest_u32_test(&mut engine, size_va), needed);
    let mut name = vec![0_u8; 64];
    engine
        .mem_read(out_va, &mut name)
        .expect("read device name");
    assert_eq!(name[needed as usize - 1], 0, "NUL terminated");

    // RIDI_DEVICEINFO writes a 32-byte RID_DEVICE_INFO.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(user32::raw_input::RIDI_DEVICEINFO),
        out_va,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoA"),
        u64::from(user32::raw_input::RID_DEVICE_INFO_SIZE)
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, out_va),
        user32::raw_input::RID_DEVICE_INFO_SIZE,
        "cbSize"
    );
    let mut dw_type = [0_u8; 4];
    engine
        .mem_read(out_va + 4, &mut dw_type)
        .expect("read dwType");
    assert_eq!(u32::from_le_bytes(dw_type), 0, "mouse");
    let mut buttons = [0_u8; 4];
    engine
        .mem_read(out_va + 12, &mut buttons)
        .expect("read dwNumberOfButtons");
    assert!(
        u32::from_le_bytes(buttons) >= 3,
        "a real mouse button count"
    );

    // The W variant reports the same count and writes UTF-16.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(user32::raw_input::RIDI_DEVICENAME),
        out_va,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoW"),
        u64::from(needed - 1)
    );

    // RIDI_PREPARSEDDATA is unsupported: WIE synthesizes no HID reports.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        device,
        u64::from(crate::guest_layout::RIDI_PREPARSEDDATA),
        out_va,
        size_va,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceInfoA"),
        0
    );
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

/// The later `WM_INPUT` lane drains per window; this lane only has to keep the
/// records addressable and ordered.
#[test]
fn test_take_pending_for_window_returns_only_that_windows_records() {
    let _guard = raw_input_test_guard();
    let raw = user32::raw_input::raw_input_state();
    raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let kbd = user32::raw_input::RawKeyboard {
        make_code: 0x1E,
        flags: crate::guest_layout::RI_KEY_MAKE,
        reserved: 0,
        vkey: 0x41,
        message: 0x0100,
        extra_information: 0,
    };
    {
        let mut guard = raw.lock().unwrap_or_else(|e| e.into_inner());
        guard.enqueue_keyboard(0xAAAA, kbd);
        guard.enqueue_keyboard(0xBBBB, kbd);
    }
    let raw = user32::raw_input::raw_input_state();
    let mut guard = raw.lock().unwrap_or_else(|e| e.into_inner());
    let taken = guard.take_pending_for_window(0xAAAA);
    assert_eq!(taken.len(), 1, "only 0xAAAA's record");
    assert_eq!(taken.first().map(|r| r.target), Some(0xAAAA));
    assert_eq!(
        taken.first().map(|r| r.header.size),
        Some(crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE)
    );
    assert_eq!(guard.take_pending_for_window(0xAAAA).len(), 0, "drained");
    assert_eq!(
        guard.take_pending_for_window(0xBBBB).len(),
        1,
        "0xBBBB intact"
    );
}

/// `DefRawInputProc` with no guest proc is a zero-LRESULT no-op, and a NULL
/// record array never reaches the callback bridge.
#[test]
fn test_def_raw_input_proc_without_a_guest_proc_returns_zero() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "DefRawInputProc"), 0);
    write_regs(&mut engine, 0x5000, 3, 0, 0, 0);
    assert_eq!(dispatch_u32(&mut engine, &mut state, "DefRawInputProc"), 0);
}

/// The public façade the later host-delivery lane calls: enqueue synthesized
/// records, ask whether a window registered a class, then drain that window's
/// records as packed bytes ready to go behind a `WM_INPUT` `lParam`.
#[test]
fn test_drain_raw_input_for_window_hands_the_lane_packed_records() {
    let _guard = raw_input_test_guard();
    let raw = user32::raw_input::raw_input_state();
    raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let hwnd = 0x4242_u64;
    // Registered through the public entry point a guest uses.
    let mut guard = raw.lock().unwrap_or_else(|e| e.into_inner());
    guard.register(user32::raw_input::RawInputDevice {
        usage_page: 1,
        usage: 6,
        flags: 0,
        target_window: hwnd,
    });
    drop(guard);
    assert!(user32::raw_input::is_raw_input_registered(hwnd, 1, 6));
    assert!(!user32::raw_input::is_raw_input_excluded(hwnd, 1, 6));
    assert!(!user32::raw_input::is_raw_input_input_sink(hwnd, 1, 6));

    user32::raw_input::enqueue_raw_keyboard(hwnd, 0x1E, 0, 0x41, 0x0100);
    user32::raw_input::enqueue_raw_mouse(hwnd, 0, 0, 0, 0, 4, -4);
    // A record for another window must not be handed to this drain.
    user32::raw_input::enqueue_raw_keyboard(0x9999, 0x1E, 0, 0x41, 0x0100);

    let drained = user32::raw_input::drain_raw_input_for_window(hwnd);
    assert_eq!(drained.len(), 2, "only 0x4242's two records");
    let kbd = drained.first().expect("keyboard record");
    assert_eq!(kbd.target, hwnd);
    assert_eq!(kbd.device_type, 1, "RIM_TYPEKEYBOARD");
    assert_eq!(kbd.device, user32::raw_input::SYNTHESIZED_KEYBOARD_DEVICE,);
    assert_eq!(kbd.wparam_code, 0, "RIM_INPUT for a focused window");
    assert_eq!(kbd.message(), 0x00FF, "WM_INPUT");
    assert_eq!(
        kbd.record_size(),
        crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE
    );
    assert_eq!(kbd.bytes.len(), 40, "24 header + 16 keyboard, unpadded");
    let mouse = drained.get(1).expect("mouse record");
    assert_eq!(mouse.device_type, 0, "RIM_TYPEMOUSE");
    assert_eq!(mouse.bytes.len(), 48, "24 header + 24 mouse, unpadded");
    assert_eq!(
        mouse.record_size(),
        crate::guest_layout::RAW_INPUT_MOUSE_RECORD_SIZE
    );

    // Draining twice delivers nothing (a record is delivered exactly once),
    // and the other window's record is untouched.
    assert!(
        user32::raw_input::drain_raw_input_for_window(hwnd).is_empty(),
        "already drained"
    );
    assert_eq!(
        user32::raw_input::drain_raw_input_for_window(0x9999).len(),
        1
    );

    // An input-sink registration (NULL target) is coded RIM_INPUTSINK.
    user32::raw_input::enqueue_raw_keyboard(0, 0x1E, 0, 0x41, 0x0100);
    let sink = user32::raw_input::drain_raw_input_for_window(0);
    assert_eq!(
        sink.first().map(|r| r.wparam_code),
        Some(1),
        "RIM_INPUTSINK for a background target"
    );
}

/// The end-to-end slice: a guest registers for raw input, the host lane
/// synthesizes a record from an input event, and the guest reads it back out
/// of the `WM_INPUT` `lParam` with `GetRawInputData` — `dwType`, `dwSize` and
/// payload included.
///
/// This drives the same three functions the winit event path calls
/// (`RegisterRawInputDevices`, `post_raw_keyboard_event` /
/// `post_raw_mouse_event`, then the guest's `GetRawInputData`), so everything
/// except the winit event itself is covered end to end.
#[test]
fn test_host_input_event_becomes_a_wm_input_the_guest_can_read_back() {
    let _guard = raw_input_test_guard();
    use crate::user32::raw_input::{
        RawMouseButton, RawMouseReport, post_raw_keyboard_event, post_raw_mouse_event,
    };
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let hwnd = 0x5150_u64;
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;

    // The guest registers keyboard + mouse for its own hwnd, the way a real
    // guest does.
    write_raw_input_devices(&mut engine, 0x4000, &[(1, 6, 0, hwnd), (1, 2, 0, hwnd)]);
    write_regs(
        &mut engine,
        0x4000,
        2,
        u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE),
        0,
        0,
    );
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "RegisterRawInputDevices"),
        1
    );

    // The host lane: one key press with focus on `hwnd` → one WM_INPUT post.
    let posts = post_raw_keyboard_event(hwnd, Some(hwnd), Some(hwnd), 0x41, true, 0x0100);
    assert_eq!(posts.len(), 1, "one WM_INPUT for the key press");
    let post = posts.first().expect("the WM_INPUT post");
    assert_eq!(post.hwnd, hwnd, "delivered to the focus window");
    assert_eq!(post.wparam, 0, "GET_RAWINPUT_CODE_WPARAM = RIM_INPUT");
    assert_ne!(post.lparam, 0, "a HRAWINPUT lParam");
    assert_ne!(post.lparam, hwnd, "the HRAWINPUT is not a guest VA");

    // The guest sizes, then copies, exactly as the documented three-call
    // protocol does — hRawInput is the lParam it received.
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(
        &mut engine,
        post.lparam,
        u64::from(user32::raw_input::RID_INPUT),
        0,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        0,
        "NULL pData only reports the size"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE,
        "24 header + 16 keyboard"
    );

    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        post.lparam,
        u64::from(user32::raw_input::RID_INPUT),
        out_va,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        u64::from(crate::guest_layout::RAW_INPUT_KEYBOARD_RECORD_SIZE)
    );
    let record = {
        let mut bytes = [0_u8; 40];
        engine.mem_read(out_va, &mut bytes).expect("read RAWINPUT");
        bytes
    };
    let word = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().unwrap_or([0; 4]));
    let short = |at: usize| u16::from_le_bytes(record[at..at + 2].try_into().unwrap_or([0; 2]));
    assert_eq!(word(0), 1, "header.dwType = RIM_TYPEKEYBOARD");
    assert_eq!(word(4), 40, "header.dwSize = the packed record size");
    assert_eq!(
        u64::from_le_bytes(record[8..16].try_into().unwrap_or([0; 8])),
        user32::raw_input::SYNTHESIZED_KEYBOARD_DEVICE,
        "header.hDevice names the synthesized keyboard"
    );
    assert_eq!(short(24), 0, "RAWKEYBOARD.MakeCode (winit reports none)");
    assert_eq!(short(26), 0, "RAWKEYBOARD.Flags = RI_KEY_MAKE for a press");
    assert_eq!(short(30), 0x41, "RAWKEYBOARD.VKey");
    assert_eq!(word(32), 0x0100, "RAWKEYBOARD.Message = WM_KEYDOWN");

    // A release codes RI_KEY_BREAK and WM_KEYUP, and the mouse class arrives
    // as a 48-byte RIM_TYPEMOUSE record at the hit-tested window.
    let release = post_raw_keyboard_event(hwnd, Some(hwnd), Some(hwnd), 0x41, false, 0x0101);
    let mouse = post_raw_mouse_event(
        hwnd,
        Some(hwnd),
        Some(hwnd),
        RawMouseReport::Button {
            button: RawMouseButton::Left,
            down: true,
            x: 12,
            y: -3,
        },
        1, // ulRawButtons = MK_LBUTTON
    );
    assert_eq!(release.len(), 1);
    assert_eq!(mouse.len(), 1);
    for (post, expected_type, expected_size) in [
        (release.first().expect("release"), 1_u32, 40_u32),
        (mouse.first().expect("mouse"), 0, 48),
    ] {
        write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
        write_regs(
            &mut engine,
            post.lparam,
            u64::from(user32::raw_input::RID_INPUT),
            out_va,
            size_va,
            0,
        );
        write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
        assert_eq!(
            dispatch_get_raw_input_data(&mut engine, &mut state),
            u64::from(expected_size)
        );
        let mut bytes = [0_u8; 48];
        engine.mem_read(out_va, &mut bytes).expect("read RAWINPUT");
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4])),
            expected_type,
            "dwType"
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            expected_size,
            "dwSize"
        );
        if expected_type == 1 {
            assert_eq!(
                u16::from_le_bytes(bytes[26..28].try_into().unwrap_or([0; 2])),
                1,
                "RI_KEY_BREAK for a release"
            );
        } else {
            assert_eq!(
                u16::from_le_bytes(bytes[28..30].try_into().unwrap_or([0; 2])),
                1,
                "RI_MOUSE_LEFT_BUTTON_DOWN"
            );
            assert_eq!(
                i32::from_le_bytes(bytes[36..40].try_into().unwrap_or([0; 4])),
                12,
                "RAWMOUSE.lLastX"
            );
        }
    }
    assert_eq!(
        release.first().map(|post| post.lparam),
        post.lparam.checked_add(1),
        "each record gets its own handle"
    );
}

/// A window that never registered gets no `WM_INPUT` at all, and a
/// `RIDEV_EXCLUDE` registration turns the legacy messages off — the two
/// halves of the registration filter the host path applies.
#[test]
fn test_registration_filter_gates_delivery_and_legacy_messages() {
    let _guard = raw_input_test_guard();
    use crate::user32::raw_input::{
        RIDEV_EXCLUDE, post_raw_keyboard_event, register_raw_input_class,
    };
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let unregistered = 0x7001_u64;
    let registered = 0x7002_u64;
    let excluded = 0x7003_u64;
    register_raw_input_class(registered, 1, 6, 0);
    register_raw_input_class(excluded, 1, 6, RIDEV_EXCLUDE);

    // No registration → no WM_INPUT, and the legacy messages stay.
    assert!(
        post_raw_keyboard_event(unregistered, None, Some(unregistered), 0x41, true, 0x0100)
            .is_empty()
    );
    assert!(!user32::raw_input::is_legacy_keyboard_input_excluded(
        unregistered
    ));

    // A plain registration → WM_INPUT, legacy messages untouched.
    let posts = post_raw_keyboard_event(registered, None, Some(registered), 0x41, true, 0x0100);
    assert_eq!(posts.len(), 1);
    assert!(!user32::raw_input::is_legacy_keyboard_input_excluded(
        registered
    ));

    // RIDEV_EXCLUDE → WM_INPUT AND the legacy WM_KEY* messages suppressed.
    let posts = post_raw_keyboard_event(excluded, None, Some(excluded), 0x41, true, 0x0100);
    assert_eq!(posts.len(), 1, "WM_INPUT still arrives");
    assert!(
        user32::raw_input::is_legacy_keyboard_input_excluded(excluded),
        "RIDEV_EXCLUDE suppresses the legacy WM_KEY* messages"
    );
    // The mouse class is untouched by a keyboard-class exclusion.
    assert!(!user32::raw_input::is_legacy_mouse_input_excluded(excluded));
}

/// `RIDEV_INPUTSINK` delivers to a background window with `RIM_INPUTSINK` and
/// withholds input from that same window while it holds focus.
#[test]
fn test_input_sink_window_only_receives_background_input() {
    let _guard = raw_input_test_guard();
    use crate::user32::raw_input::{
        RIDEV_INPUTSINK, post_raw_keyboard_event, register_raw_input_class,
    };
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let sink = 0x7100_u64;
    let other = 0x7101_u64;
    register_raw_input_class(sink, 1, 6, RIDEV_INPUTSINK);

    // Focused: the sink registration must not deliver (winuser.h:6471).
    assert!(
        post_raw_keyboard_event(sink, Some(sink), Some(sink), 0x41, true, 0x0100).is_empty(),
        "a focused input-sink window gets no input"
    );

    // Unfocused: it gets exactly one post, coded RIM_INPUTSINK.
    let posts = post_raw_keyboard_event(other, Some(sink), Some(other), 0x41, true, 0x0100);
    assert_eq!(posts.len(), 1, "the background sink window is served");
    let post = posts.first().expect("the sink post");
    assert_eq!(post.hwnd, sink, "delivered to the sink window");
    assert_eq!(post.wparam, 1, "RIM_INPUTSINK");
}

/// A guest that polls `GetRawInputBuffer` gets both records packed back to
/// back, each `dwSize` its own packed size, so `NEXTRAWINPUTBLOCK` lands on
/// the second record's header — the chaining a raw-input app depends on.
#[test]
fn test_wm_input_records_chain_in_a_get_raw_input_buffer_fill() {
    let _guard = raw_input_test_guard();
    use crate::user32::raw_input::{RawMouseButton, RawMouseReport, post_raw_mouse_event};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let hwnd = 0x7200_u64;
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    // The delivery filter only serves a window registered for the device
    // class, so register the mouse class (usage page 1, usage 2) before
    // posting — an unregistered window gets no WM_INPUT by design.
    user32::raw_input::register_raw_input_class(hwnd, 1, 2, 0);

    // Two host mouse events. Their WM_INPUT posts are what a real host posts;
    // the records themselves are still queued for the buffer path.
    for report in [
        RawMouseReport::Movement { dx: 3, dy: -4 },
        RawMouseReport::Button {
            button: RawMouseButton::Right,
            down: false,
            x: 7,
            y: 9,
        },
    ] {
        let posts = post_raw_mouse_event(hwnd, Some(hwnd), Some(hwnd), report, 0);
        assert_eq!(posts.len(), 1, "one WM_INPUT per host event");
    }

    // Size query, then the fill: 48 + 48 with no inter-record padding.
    write_guest_u32(&mut engine, size_va, 0).expect("write guest u32");
    write_regs(&mut engine, 0, size_va, 0, 0, 0);
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        0,
        "NULL pData reports the size"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        96,
        "two 48-byte mouse records"
    );
    write_guest_u32(&mut engine, size_va, 128).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        size_va,
        u64::from(user32::raw_input::RAW_INPUT_HEADER_SIZE),
        0,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        2,
        "both records returned"
    );
    let mut out = [0_u8; 96];
    engine.mem_read(out_va, &mut out).expect("read the buffer");
    let word = |r: &[u8], at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap_or([0; 4]));
    let short =
        |r: &[u8], at: usize| u16::from_le_bytes(r[at..at + 2].try_into().unwrap_or([0; 2]));
    assert_eq!(word(&out, 0), 0, "record 0 dwType = mouse");
    assert_eq!(word(&out, 4), 48, "record 0 dwSize");
    // NEXTRAWINPUTBLOCK: RAWINPUT_ALIGN(0 + 48) == 48, i.e. no padding.
    assert_eq!(word(&out, 48), 0, "record 1 dwType at +48");
    assert_eq!(word(&out, 52), 48, "record 1 dwSize");
    assert_eq!(short(&out, 24), 0, "record 0 usFlags = MOUSE_MOVE_RELATIVE");
    assert_eq!(
        i32::from_le_bytes(out[36..40].try_into().unwrap_or([0; 4])),
        3,
        "record 0 lLastX = the movement delta"
    );
    assert_eq!(
        short(&out, 72),
        1,
        "MOUSE_MOVE_ABSOLUTE for a button report"
    );
    assert_eq!(short(&out, 76), 8, "RI_MOUSE_RIGHT_BUTTON_UP");
    assert_eq!(
        i32::from_le_bytes(out[84..88].try_into().unwrap_or([0; 4])),
        7,
        "record 1 lLastX"
    );
    // RAWMOUSE lLastY is +16 in the payload (winuser.h:6314), i.e. +40 from the
    // record's own base, and record 1 starts at +48.
    assert_eq!(
        i32::from_le_bytes(out[88..92].try_into().unwrap_or([0; 4])),
        9,
        "record 1 lLastY"
    );
}

/// `GetRawInputBuffer` and the `WM_INPUT` `lParam` are two views of ONE buffered
/// input, with independent lifetimes: a successful fill consumes the buffer
/// view, so a second fill sees nothing, but the `HRAWINPUT` the guest is still
/// holding stays resolvable through `GetRawInputData`. The record is counted
/// once, by the fill that took it.
#[test]
fn test_get_raw_input_buffer_drain_leaves_the_hrawinput_readable() {
    let _guard = raw_input_test_guard();
    use crate::user32::raw_input::{RawMouseButton, RawMouseReport, post_raw_mouse_event};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    user32::raw_input::raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let hwnd = 0x7300_u64;
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    user32::raw_input::register_raw_input_class(hwnd, 1, 2, 0);

    let posts = post_raw_mouse_event(
        hwnd,
        Some(hwnd),
        Some(hwnd),
        RawMouseReport::Button {
            button: RawMouseButton::Left,
            down: true,
            x: 4,
            y: 6,
        },
        1,
    );
    let lparam = posts.first().expect("the WM_INPUT post").lparam;

    // The fill takes the one buffered record, once.
    write_guest_u32(&mut engine, size_va, 128).expect("write guest u32");
    write_regs(
        &mut engine,
        out_va,
        size_va,
        u64::from(user32::raw_input::RAW_INPUT_HEADER_SIZE),
        0,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_buffer(&mut engine, &mut state),
        1,
        "the buffered record is returned"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, size_va),
        crate::guest_layout::RAW_INPUT_MOUSE_RECORD_SIZE
    );

    // Consumed: a second fill is empty, and the record is not counted twice.
    assert_eq!(
        dispatch_def_raw_input_buffer(&mut engine, &mut state, 0, &size_va),
        0,
        "the buffer view was drained"
    );
    assert_eq!(read_guest_u32_test(&mut engine, size_va), 0, "nothing left");

    // The handle view is untouched by that drain.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        lparam,
        u64::from(user32::raw_input::RID_INPUT),
        out_va,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        u64::from(crate::guest_layout::RAW_INPUT_MOUSE_RECORD_SIZE),
        "the drained record's HRAWINPUT still resolves"
    );
    assert_eq!(
        i32::from_le_bytes(
            read_guest_bytes_test(&mut engine, out_va, 48)[36..40]
                .try_into()
                .unwrap_or([0; 4])
        ),
        4,
        "lLastX survives the drain"
    );
}

/// Read `len` guest bytes at `addr` (test helper for the record assertions
/// above).
fn read_guest_bytes_test(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    engine.mem_read(addr, &mut bytes).expect("read guest bytes");
    bytes
}

/// A `HRAWINPUT` WIE no longer holds is a normal bad-handle failure, not a
/// silent copy of whatever guest memory happens to sit at that address.
#[test]
fn test_get_raw_input_data_rejects_an_unknown_hrawinput() {
    let _guard = raw_input_test_guard();
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let size_va = 0x3000_u64;
    let out_va = 0x5000_u64;
    // Guest memory at the address holds a plausible header, but the handle was
    // never handed out, so the guest-memory fallback would copy it. Windows
    // would fail here too — but the point of the pin is that WIE's delivered
    // handles are resolvable, so an unknown one may legitimately resolve as a
    // guest pointer (a GetRawInputBuffer slot). Use an address no guest buffer
    // can occupy instead: the fake handle space with nothing published.
    write_guest_u32(&mut engine, size_va, 64).expect("write guest u32");
    write_regs(
        &mut engine,
        0x0000_0000_6600_0600,
        u64::from(user32::raw_input::RID_INPUT),
        out_va,
        size_va,
        0,
    );
    write_stack_arg5(&mut engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    assert_eq!(
        dispatch_get_raw_input_data(&mut engine, &mut state),
        u64::from(u32::MAX),
        "a handle in the HRAWINPUT space that was never published fails"
    );
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

/// Write a `RAWINPUTDEVICE[]` array the way a guest does.
#[allow(clippy::too_many_arguments)]
fn write_raw_input_devices(engine: &mut IcedCpu, base: u64, entries: &[(u16, u16, u32, u64)]) {
    for (i, (usage_page, usage, flags, target)) in entries.iter().enumerate() {
        let stride = u64::from(user32::raw_input::RAW_INPUT_DEVICE_SIZE);
        let index = u64::try_from(i).unwrap_or(0);
        let va = base + index * stride;
        let mut bytes = [0_u8; 16];
        bytes[0..2].copy_from_slice(&usage_page.to_le_bytes());
        bytes[2..4].copy_from_slice(&usage.to_le_bytes());
        bytes[4..8].copy_from_slice(&flags.to_le_bytes());
        bytes[8..16].copy_from_slice(&target.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write RAWINPUTDEVICE");
    }
}

/// Write the 5th Win64 stack argument (the Win64 shadow-space slot).
fn write_stack_arg5(engine: &mut IcedCpu, value: u32) {
    engine
        .mem_write(super::STACK_TOP + 0x28, &u64::from(value).to_le_bytes())
        .expect("write 5th stack arg");
}

/// Whether `hwnd` excluded the (usage_page, usage) class from legacy messages.
fn guard_is_excluded(
    raw: &std::sync::Mutex<user32::raw_input::RawInputState>,
    hwnd: u64,
    usage_page: u16,
    usage: u16,
) -> bool {
    raw.lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_excluded(hwnd, usage_page, usage)
}

/// `GetRawInputBuffer` with `pData == NULL` and `count` ignored — only used to
/// observe the "no records" case.
fn dispatch_def_raw_input_buffer(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    _unused: u64,
    size_va: &u64,
) -> u64 {
    write_guest_u32(engine, *size_va, 0).expect("write guest u32");
    write_regs(engine, 0, *size_va, 0, 0, 0);
    write_stack_arg5(engine, user32::raw_input::RAW_INPUT_HEADER_SIZE);
    dispatch_get_raw_input_buffer(engine, state)
}

/// `GetRawInputBuffer` through the string-dispatch entry point.
fn dispatch_get_raw_input_buffer(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    user32::dispatch_user32_extra(&mut ctx, "GetRawInputBuffer")
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

/// `GetRawInputData` through the string-dispatch entry point.
fn dispatch_get_raw_input_data(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    user32::dispatch_user32_extra(&mut ctx, "GetRawInputData")
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

/// Read a guest u32 at `addr` (test-side mirror of `read_guest_u32`).
fn read_guest_u32_test(engine: &mut IcedCpu, addr: u64) -> u32 {
    let mut b = [0_u8; 4];
    engine.mem_read(addr, &mut b).expect("read guest u32");
    u32::from_le_bytes(b)
}

/// Read a guest u16 at `addr`.
fn read_guest_u16_test(engine: &mut IcedCpu, addr: u64) -> u16 {
    let mut b = [0_u8; 2];
    engine.mem_read(addr, &mut b).expect("read guest u16");
    u16::from_le_bytes(b)
}

/// `EnumDisplaySettingsW` writes the Win64 `DEVMODEW` fields at the real
/// offsets SDL2 reads (`WIN_GetDisplayModeFromDevMode`): `dmSize` @0x44 = 220,
/// `dmBitsPerPel` @0xA8, `dmPelsWidth` @0xAC, `dmPelsHeight` @0xB0,
/// `dmDisplayFrequency` @0xB8. The old stub wrote them at 0x16..0x2C, which SDL
/// reads as zeros → a 0×0 @0bpp desktop mode that failed `WIN_InitModes`.
#[test]
fn test_enum_display_settings_w_writes_real_devmodew_offsets() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let mode_va = 0x7000_u64;
    // rcx=device (ignored), rdx=mode 0, r8=mode_va.
    write_regs(&mut engine, 0, 0, mode_va, 0, 0);
    let r = user32::handle_enum_display_settings_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EnumDisplaySettingsW must dispatch");
    assert_eq!(r.return_value, 1, "mode 0 enumerates");

    assert_eq!(
        read_guest_u16_test(&mut engine, mode_va + 0x44),
        220,
        "dmSize @0x44"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0xA8),
        32,
        "dmBitsPerPel @0xA8"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0xAC),
        1920,
        "dmPelsWidth @0xAC"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0xB0),
        1080,
        "dmPelsHeight @0xB0"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0xB8),
        60,
        "dmDisplayFrequency @0xB8"
    );

    // ENUM_CURRENT_SETTINGS (-1) reports the same desktop mode.
    write_regs(&mut engine, 0, u64::from(u32::MAX), mode_va, 0, 0);
    let r = user32::handle_enum_display_settings_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EnumDisplaySettingsW (current) must dispatch");
    assert_eq!(r.return_value, 1, "ENUM_CURRENT_SETTINGS enumerates");
    assert_eq!(read_guest_u32_test(&mut engine, mode_va + 0xAC), 1920);

    // Any other mode is the exhausted end of the enumeration.
    write_regs(&mut engine, 0, 5, mode_va, 0, 0);
    let r = user32::handle_enum_display_settings_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EnumDisplaySettingsW (mode 5) must dispatch");
    assert_eq!(r.return_value, 0, "mode 5 is past the end");
}

/// `EnumDisplaySettingsA` writes the Win64 `DEVMODEA` fields (the ANSI struct's
/// CHAR name fields are half the W width, so every offset after the device name
/// is 0x20 less than `DEVMODEW`): `dmSize` @0x24 = 156, `dmBitsPerPel` @0x68,
/// `dmPelsWidth` @0x6C, `dmPelsHeight` @0x70, `dmDisplayFrequency` @0x78.
#[test]
fn test_enum_display_settings_a_writes_real_devmodea_offsets() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let mode_va = 0x7000_u64;
    write_regs(&mut engine, 0, 0, mode_va, 0, 0);
    let r = user32::handle_enum_display_settings_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EnumDisplaySettingsA must dispatch");
    assert_eq!(r.return_value, 1, "mode 0 enumerates");

    assert_eq!(
        read_guest_u16_test(&mut engine, mode_va + 0x24),
        156,
        "dmSize @0x24"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0x68),
        32,
        "dmBitsPerPel @0x68"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0x6C),
        1920,
        "dmPelsWidth @0x6C"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0x70),
        1080,
        "dmPelsHeight @0x70"
    );
    assert_eq!(
        read_guest_u32_test(&mut engine, mode_va + 0x78),
        60,
        "dmDisplayFrequency @0x78"
    );
}
