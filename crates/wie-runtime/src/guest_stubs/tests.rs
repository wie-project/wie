use super::config::{
    CLOCK_TABLE_SLOT_FILETIME, CLOCK_TABLE_SLOT_QPC, CLOCK_TABLE_SLOT_QPC_FREQ,
    CLOCK_TABLE_SLOT_TICK64, CLOCK_TABLE_SLOT_TIME,
};
use super::*;

/// GetLastError/SetLastError plant GS-relative stubs so every thread reads
/// or writes ITS OWN TEB last-error slot (the GS base resolves per engine).
/// The body embeds `TEB_LAST_ERROR_OFFSET` as a disp32 — ModRM 04 + SIB 25
/// forces the base-less form (mod=00 rm=101 would be RIP-relative in x86-64).
#[test]
fn last_error_stubs_are_gs_relative_per_thread() {
    let cfg = GuestStubConfig::CLASSIFY_ONLY;

    let get =
        classify_guest_stub("KERNEL32.dll", "GetLastError", &cfg).expect("GetLastError classifies");
    assert_eq!(get, GuestStubKind::LoadLastError);
    let body = get.encode(&cfg);
    assert_eq!(
        body,
        vec![0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3],
        "GetLastError must be mov eax, [gs:0x68]; ret"
    );

    let set =
        classify_guest_stub("KERNEL32.dll", "SetLastError", &cfg).expect("SetLastError classifies");
    assert_eq!(set, GuestStubKind::StoreLastError);
    let body = set.encode(&cfg);
    assert_eq!(
        body,
        vec![0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3],
        "SetLastError must be mov [gs:0x68], ecx; ret"
    );

    // No embedded guest VA: the body is config-independent and fits the IAT stride.
    assert!(!get.needs_real_guest_addresses());
    assert!(!set.needs_real_guest_addresses());
    assert!(!get.needs_out_of_line_helper());
    assert!(!set.needs_out_of_line_helper());
    // The displacement matches the shared guest-layout constant.
    assert_eq!(
        u32::try_from(wie_cpu::guest_layout::TEB_LAST_ERROR_OFFSET).unwrap_or(0),
        0x68
    );
}

#[test]
fn get_cwd_stub_encodes_and_patches_rel8() {
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let body = GuestStubKind::GetCurrentDirectoryW {
        cwd_blob_va: cfg.cwd_blob_va,
    }
    .encode(&cfg);
    assert!(body.len() > 20);
    assert_eq!(*body.last().unwrap(), 0xc3);
}

#[test]
fn dialog_box_stub_encodes_modal_loop() {
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let body = GuestStubKind::DialogBoxParam {
        create_dialog_param_va: cfg.create_dialog_param_a_va,
        get_message_va: cfg.get_message_a_va,
        is_dialog_message_va: cfg.is_dialog_message_a_va,
        dispatch_message_va: cfg.dispatch_message_a_va,
        dialog_result_va: cfg.dialog_result_va,
    }
    .encode(&cfg);
    // ~120 bytes of modal loop; must end in `ret`.
    assert!(body.len() > 80, "dialog stub too short: {}", body.len());
    assert!(body.len() < 200, "dialog stub too long: {}", body.len());
    assert_eq!(*body.last().unwrap(), 0xc3);
    // The failure path (CreateDialogParam → 0) must return -1 in RAX
    // (0x48 0xc7 0xc0 0xff.. = `mov rax, -1`) and skip the result load.
    let neg1 = [0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff];
    assert!(
        body.windows(neg1.len()).any(|w| w == neg1),
        "dialog stub must materialize -1 on CreateDialogParam failure"
    );
    // The result must be loaded via `mov eax, [rax]` after a `mov rax, imm`.
    let load = [0x8b, 0x00, 0xc3];
    assert!(
        body.windows(load.len()).any(|w| w == load) || body.windows(2).any(|w| w == [0x8b, 0x00]),
        "dialog stub must end with the dialog-result load"
    );
}

#[test]
fn dialog_box_stub_forwards_init_param_at_the_callee_read_offset() {
    // The stub forwards DialogBoxParam's 5th arg (dwInitParam) as
    // CreateDialogParam's 5th arg. The CALLER places arg5 at [rsp+0x20] so
    // the callee reads it at [rsp+0x28] after the call pushes the return
    // address. A store at [rsp+0x28] (as originally written) put the value 8
    // bytes too high — the guest dialog proc then read garbage lParam and
    // faulted on `s_pGotoData->iLine` in WM_INITDIALOG.
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let body = GuestStubKind::DialogBoxParam {
        create_dialog_param_va: cfg.create_dialog_param_a_va,
        get_message_va: cfg.get_message_a_va,
        is_dialog_message_va: cfg.is_dialog_message_a_va,
        dispatch_message_va: cfg.dispatch_message_a_va,
        dialog_result_va: cfg.dialog_result_va,
    }
    .encode(&cfg);
    // `mov rax, [rsp+0x90]` (load the caller's 5th arg) immediately followed
    // by `mov [rsp+0x20], rax` (store it in CreateDialogParam's arg5 slot).
    let forward = [
        0x48, 0x8b, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00, // mov rax, [rsp+0x90]
        0x48, 0x89, 0x44, 0x24, 0x20, // mov [rsp+0x20], rax
    ];
    assert!(
        body.windows(forward.len()).any(|w| w == forward),
        "dialog stub must place dwInitParam at [rsp+0x20] (callee reads \
         [rsp+0x28] after the call pushes the return address)"
    );
    // And it must NOT store at [rsp+0x28] (the old 8-bytes-too-high offset).
    let wrong = [
        0x48, 0x8b, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00, 0x48, 0x89, 0x44, 0x24, 0x28,
    ];
    assert!(
        !body.windows(wrong.len()).any(|w| w == wrong),
        "dialog stub must not store dwInitParam at [rsp+0x28] (off by 8)"
    );
}

#[test]
fn dialog_callee_vas_resolve_without_imports() {
    // A guest that imports ONLY DialogBoxParamA never imports the modal
    // loop's callees; their fake VAs must still decode to the right
    // WinApiIds (deterministic encode_export, stop-bit default host-stop).
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let expect_export = |va: u64, id: wie_winapi::WinApiId| {
        assert!(
            va >= wie_winapi::FAKE_API_BASE,
            "callee VA {va:#x} outside fake-API window"
        );
        assert_eq!(
            wie_winapi::decode_fake_va(va),
            Some(wie_winapi::FakeVa::Export(id)),
            "callee VA {va:#x} does not decode to {id:?}"
        );
    };
    expect_export(
        cfg.create_dialog_param_a_va,
        wie_winapi::WinApiId::User32Createdialogparama,
    );
    expect_export(
        cfg.create_dialog_param_w_va,
        wie_winapi::WinApiId::User32Createdialogparamw,
    );
    expect_export(
        cfg.get_message_a_va,
        wie_winapi::WinApiId::User32Getmessagea,
    );
    expect_export(
        cfg.is_dialog_message_a_va,
        wie_winapi::WinApiId::User32Isdialogmessagea,
    );
    expect_export(
        cfg.dispatch_message_a_va,
        wie_winapi::WinApiId::User32Dispatchmessagea,
    );
    // The dialog-result slot lives inside the mapped stub data page.
    assert!(
        cfg.dialog_result_va >= crate::memory::DEFAULT_LAYOUT.guest_stub_data.base
            && cfg.dialog_result_va
                < crate::memory::DEFAULT_LAYOUT.guest_stub_data.base
                    + crate::memory::DEFAULT_LAYOUT.guest_stub_data.size as u64
    );
    // CLASSIFY_ONLY must differ from the real config (forces the
    // needs_real_guest_addresses re-classification path).
    let classify_only = GuestStubConfig::CLASSIFY_ONLY;
    assert_ne!(classify_only.dialog_result_va, cfg.dialog_result_va);
}

#[test]
fn metrics_table_matches_known_sm() {
    let page = build_stub_data_page(wie_winapi::DisplayMetrics::default());
    // SM_CXSCREEN = 0 → default width
    assert_eq!(&page[0..4], &1920_u32.to_le_bytes());
    // SM_CYSCREEN = 1 → default height
    assert_eq!(&page[4..8], &1080_u32.to_le_bytes());
    // Non-default metrics flow through: a 1728×1117 monitor is reported as-is.
    let page = build_stub_data_page(wie_winapi::DisplayMetrics::new(1728, 1117));
    assert_eq!(&page[0..4], &1728_u32.to_le_bytes());
    assert_eq!(&page[4..8], &1117_u32.to_le_bytes());
}

#[test]
fn classify_langid_and_not_virtual_protect() {
    let cfg = GuestStubConfig::CLASSIFY_ONLY;
    assert!(matches!(
        classify_guest_stub("KERNEL32.dll", "GetSystemDefaultLangID", &cfg),
        Some(GuestStubKind::ReturnImm32(0x0409))
    ));
    assert!(classify_guest_stub("KERNEL32.dll", "VirtualProtect", &cfg).is_none());
    assert!(classify_guest_stub("KERNEL32.dll", "VirtualQuery", &cfg).is_none());
    assert!(classify_guest_stub("KERNEL32.dll", "LocalAlloc", &cfg).is_none());
    // Critical sections must not be VoidRet guest stubs.
    assert!(classify_guest_stub("KERNEL32.dll", "EnterCriticalSection", &cfg).is_none());
    assert!(classify_guest_stub("KERNEL32.dll", "LeaveCriticalSection", &cfg).is_none());
    assert!(classify_guest_stub("KERNEL32.dll", "DeleteCriticalSection", &cfg).is_none());
}

/// Every B5 clock API classifies to a table-reading stub at the matching
/// slot offset, and every such stub embeds the table VA (so it is
/// re-derived with the real config at plant time).
#[test]
fn clock_stubs_classify_to_table_slots() {
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let base = cfg.clock_table_va;
    assert_ne!(base, 0, "clock table must live at a real guest VA");

    let tick =
        classify_guest_stub("KERNEL32.dll", "GetTickCount", &cfg).expect("GetTickCount classifies");
    assert_eq!(tick, GuestStubKind::LoadZx32FromVa(base));

    let tick64 = classify_guest_stub("KERNEL32.dll", "GetTickCount64", &cfg)
        .expect("GetTickCount64 classifies");
    assert_eq!(
        tick64,
        GuestStubKind::LoadZx64FromVa(base.saturating_add(CLOCK_TABLE_SLOT_TICK64))
    );

    let time =
        classify_guest_stub("winmm.dll", "timeGetTime", &cfg).expect("timeGetTime classifies");
    assert_eq!(
        time,
        GuestStubKind::LoadZx32FromVa(base.saturating_add(CLOCK_TABLE_SLOT_TIME))
    );

    let ft = classify_guest_stub("KERNEL32.dll", "GetSystemTimeAsFileTime", &cfg)
        .expect("GetSystemTimeAsFileTime classifies");
    assert_eq!(
        ft,
        GuestStubKind::CopyU64FromVaToRcxPtr {
            slot_va: base.saturating_add(CLOCK_TABLE_SLOT_FILETIME),
        }
    );

    let qpc = classify_guest_stub("KERNEL32.dll", "QueryPerformanceCounter", &cfg)
        .expect("QueryPerformanceCounter classifies");
    assert_eq!(
        qpc,
        GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: base.saturating_add(CLOCK_TABLE_SLOT_QPC),
        }
    );

    let qpf = classify_guest_stub("KERNEL32.dll", "QueryPerformanceFrequency", &cfg)
        .expect("QueryPerformanceFrequency classifies");
    assert_eq!(
        qpf,
        GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: base.saturating_add(CLOCK_TABLE_SLOT_QPC_FREQ),
        }
    );

    for kind in [tick, tick64, time, ft, qpc, qpf] {
        assert!(
            kind.needs_real_guest_addresses(),
            "{kind:?} embeds the table VA and must be re-derived at plant time"
        );
        // Encoded bodies are self-contained machine code ending in `ret`.
        let body = kind.encode(&cfg);
        assert_eq!(body.last(), Some(&0xc3), "{kind:?} must end in ret");
    }
}

/// End-to-end refresh: the 6×u64 table lands in guest memory at the layout
/// VA, advances across a 10 ms sleep, and never moves backwards.
#[test]
fn refresh_clock_table_writes_advancing_slots() {
    use wie_cpu::CpuBackend;
    let backend = wie_cpu::open_cpu().expect("cpu backend opens");
    let (mut engine, _shared, _guest_mem) = match backend {
        CpuBackend::Jit { engine, shared } => (engine, Some(shared), None),
        CpuBackend::Iced { engine, guest_mem } => (engine, None, Some(guest_mem)),
    };
    let layout = crate::memory::DEFAULT_LAYOUT;
    engine
        .mem_map(
            layout.clock_table.base,
            layout.clock_table.size,
            wie_cpu::RwxPerms::READ_WRITE,
        )
        .expect("map clock table");

    let read_slot = |engine: &mut dyn wie_cpu::CpuEngine, slot: u64| -> u64 {
        let mut buf = [0_u8; 8];
        let _ = engine.mem_read(layout.clock_table.base.saturating_add(slot), &mut buf);
        u64::from_le_bytes(buf)
    };

    refresh_clock_table(&mut *engine, layout.clock_table.base).expect("first refresh");
    let first_tick = read_slot(&mut *engine, CLOCK_TABLE_SLOT_TICK64);
    let first_qpc = read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC);
    assert_eq!(
        read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC_FREQ),
        10_000_000,
        "QPC frequency slot must be the fixed 10 MHz base"
    );

    std::thread::sleep(std::time::Duration::from_millis(10));
    refresh_clock_table(&mut *engine, layout.clock_table.base).expect("second refresh");
    let second_tick = read_slot(&mut *engine, CLOCK_TABLE_SLOT_TICK64);
    let second_qpc = read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC);

    assert!(
        second_tick >= first_tick.saturating_add(8),
        "tick_count_64 slot did not advance: {first_tick} -> {second_tick}"
    );
    assert!(
        second_qpc >= first_qpc,
        "qpc slot went backwards: {second_qpc} < {first_qpc}"
    );
}

/// Every known guest-stub classification: if the `CLASSIFY_ONLY` body differs
/// from the real-config body, the kind **must** declare that it needs
/// re-classification via [`GuestStubKind::needs_real_guest_addresses`].
///
/// This catches cases where a new stub kind embeds a config-dependent guest
/// address but the author forgets to add the variant to the `matches!` list
/// in `needs_real_guest_addresses`. Without this guard the stub would be
/// planted with address zero for all cfg-derived addresses.
#[test]
fn every_stub_needing_real_addresses_is_listed() {
    // Every (library, name) pair that `classify_guest_stub` can return `Some` for.
    // When a new stub is added to `classify_guest_stub`, add it here too.
    let stubs: &[(&str, &str)] = &[
        // UCRT / CRT
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__acrt_iob_func"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_initterm"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_initterm_e"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "fflush"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "setvbuf"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_crt_atexit"),
        (
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "_set_invalid_parameter_handler",
        ),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_set_app_type"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_set_new_mode"),
        (
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "_configure_narrow_argv",
        ),
        (
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "_initialize_narrow_environment",
        ),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__setusermatherr"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_configthreadlocale"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "_cexit"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "signal"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__environ"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argv"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argc"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__commode"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__fmode"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__acmdln"),
        // USER32
        ("USER32.dll", "GetSystemMetrics"),
        ("USER32.dll", "GetSysColor"),
        ("USER32.dll", "GetSysColorBrush"),
        ("USER32.dll", "GetDesktopWindow"),
        ("USER32.dll", "DialogBoxParamA"),
        ("USER32.dll", "DialogBoxParamW"),
        // KERNEL32 / ntdll
        ("KERNEL32.dll", "EncodePointer"),
        ("KERNEL32.dll", "DecodePointer"),
        ("KERNEL32.dll", "GetLastError"),
        ("KERNEL32.dll", "SetLastError"),
        ("KERNEL32.dll", "FlsGetValue"),
        ("KERNEL32.dll", "FlsSetValue"),
        ("KERNEL32.dll", "SetHandleCount"),
        ("KERNEL32.dll", "GetCurrentProcessId"),
        ("KERNEL32.dll", "GetCurrentThreadId"),
        ("KERNEL32.dll", "IsDebuggerPresent"),
        ("KERNEL32.dll", "GetACP"),
        ("KERNEL32.dll", "GetOEMCP"),
        ("KERNEL32.dll", "GetSystemDefaultLangID"),
        ("KERNEL32.dll", "GetUserDefaultLangID"),
        ("KERNEL32.dll", "GetCurrentProcess"),
        ("KERNEL32.dll", "GetProcessHeap"),
        ("KERNEL32.dll", "GetCommandLineA"),
        ("KERNEL32.dll", "GetCommandLineW"),
        ("KERNEL32.dll", "GetCurrentDirectoryW"),
        // Clock stubs read the host-written guest clock table.
        ("KERNEL32.dll", "GetTickCount"),
        ("KERNEL32.dll", "GetTickCount64"),
        ("KERNEL32.dll", "GetSystemTimeAsFileTime"),
        ("KERNEL32.dll", "QueryPerformanceCounter"),
        ("KERNEL32.dll", "QueryPerformanceFrequency"),
        ("winmm.dll", "timeGetTime"),
    ];

    let real_cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);

    for &(library, name) in stubs {
        let kind0 = classify_guest_stub(library, name, &GuestStubConfig::CLASSIFY_ONLY);
        let kind1 = classify_guest_stub(library, name, &real_cfg);

        match (kind0, kind1) {
            (Some(k0), Some(k1)) => {
                let body0 = k0.encode(&GuestStubConfig::CLASSIFY_ONLY);
                let body1 = k1.encode(&real_cfg);
                if body0 != body1 {
                    assert!(
                        k0.needs_real_guest_addresses(),
                        "GuestStubKind variant for {library}!{name} produces \
                         different machine code with CLASSIFY_ONLY vs real config \
                         (k0={k0:?}, k1={k1:?}). \
                         Add this variant to `needs_real_guest_addresses()`.",
                    );
                }
            }
            (Some(_), None) => {
                panic!("{library}!{name}: classifies with CLASSIFY_ONLY but not with real cfg");
            }
            (None, Some(_)) => {
                panic!("{library}!{name}: classifies with real cfg but not with CLASSIFY_ONLY");
            }
            (None, None) => {
                panic!("{library}!{name}: no longer classifies as a guest stub; remove from test");
            }
        }
    }
}

#[test]
fn file_dialog_loop_stub_encodes_modal_loop() {
    let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
    let mut buf = Vec::new();
    let mut ctx = StubCtx::new(&mut buf, &cfg);
    ctx.encode_file_dialog_loop();
    let body = buf;
    assert!(body.len() > 80, "loop too short: {}", body.len());
    assert!(body.len() < 200, "loop too long: {}", body.len());
    assert_eq!(*body.last().unwrap(), 0xc3, "ends in ret");
    // The loop must preserve the callback's dialog HWND (rcx → [rsp+0x20]).
    let save_hwnd = [0x48, 0x89, 0x4c, 0x24, 0x20];
    assert!(
        body.windows(save_hwnd.len()).any(|w| w == save_hwnd),
        "loop must save the callback dialog hwnd"
    );
    // It must load the result via `mov eax, [rax]` after `mov rax, imm64`.
    let load = [0x8b, 0x00];
    assert!(
        body.windows(load.len()).any(|w| w == load),
        "loop must return the dialog-result slot"
    );
}

#[test]
fn file_dialog_proc_stub_encodes_end_dialog_decision() {
    let end_dialog_va = 0x0000_7000_0000_2d80;
    let body = encode_file_dialog_proc(end_dialog_va);
    assert!(body.len() > 30, "proc stub too short: {}", body.len());
    assert_eq!(*body.last().unwrap(), 0xc3);
    // The stub must embed the EndDialog fake VA (`mov rax, imm64`).
    let mut needle = Vec::new();
    needle.extend_from_slice(&[0x48, 0xb8]);
    needle.extend_from_slice(&end_dialog_va.to_le_bytes());
    assert!(
        body.windows(needle.len()).any(|w| w == needle.as_slice()),
        "proc stub must call EndDialog"
    );
    // WM_COMMAND low-word compare for IDOK (cmp eax, 1) and IDCANCEL (cmp eax, 2).
    assert!(
        body.windows(3).any(|w| w == [0x83, 0xf8, 0x01]),
        "proc stub must recognize IDOK"
    );
    assert!(
        body.windows(3).any(|w| w == [0x83, 0xf8, 0x02]),
        "proc stub must recognize IDCANCEL"
    );
}

/// The shared file/font dialog proc stub's IDOK branch must JUMP to the
/// EndDialog block, not fall through into the Strikeout sentinel branch.
///
/// Regression: the OK branch ended in `mov edx, 1` with no jump, so execution
/// continued into `.strikeout` (`mov edx, strikeout_id`), overwriting the
/// result — the font dialog's OK button then toggled Strikeout instead of
/// closing (ghost-modal: the first File→Exit after "OK" was eaten by the
/// still-open modal loop).
#[test]
fn file_dialog_proc_stub_ok_branch_jumps_to_close() {
    let body = encode_file_dialog_proc(0x0000_7000_0000_2d80);
    // `mov edx, 1` — the IDOK result load (imm32 = 1 is unique in the body).
    let ok_load = [0xba, 0x01, 0x00, 0x00, 0x00];
    let found = body
        .windows(ok_load.len())
        .enumerate()
        .find(|(_, w)| *w == ok_load)
        .map(|(i, _)| i)
        .expect("IDOK branch must load result 1 into edx");
    assert_eq!(
        body.get(found + 5),
        Some(&0xeb),
        "IDOK branch must jump after `mov edx, 1` — a fall-through would run \
         the Strikeout branch and overwrite the result"
    );
    // The short-jump target must be the EndDialog block (`.close` starts with
    // `sub rsp, 0x28`), not the strikeout load.
    let rel8 = i8::from_le_bytes([body[found + 6]]);
    let target = i64::try_from(found + 7)
        .unwrap_or(0)
        .saturating_add(i64::from(rel8));
    let target = usize::try_from(target).expect("jump target in bounds");
    assert_eq!(
        body.get(target..target + 4),
        Some(&[0x48, 0x83, 0xec, 0x28][..]),
        "IDOK jump must land on the EndDialog call block"
    );
}
