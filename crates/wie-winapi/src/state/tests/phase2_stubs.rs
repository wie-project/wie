//! Phase-2 stub-wave tests: the DOOM Retro / SDL2 boot-surface handlers added
//! in the batch (critical-section Ex/Try, mutex, SList, version, power status,
//! trivial kernel32 returns, the user32 geometry/clipboard/keyboard stubs and
//! the real math helpers IntersectRect / PtInRect).
use super::*;

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
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "RegisterRawInputDevices"),
        0,
        "no raw-input devices → FALSE so SDL falls back to WM_MOUSE*"
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

#[test]
fn test_raw_input_device_list_is_empty() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let count_va = 0x3000_u64;
    write_regs(&mut engine, 0, count_va, 0, 0, 0);
    assert_eq!(
        dispatch_u32(&mut engine, &mut state, "GetRawInputDeviceList"),
        0,
        "no raw-input devices"
    );
    let mut n = [0_u8; 4];
    engine.mem_read(count_va, &mut n).expect("count");
    assert_eq!(u32::from_le_bytes(n), 0);
}
