//! Shell32 tests: CommandLineToArgvW and DragAcceptFiles.
use super::*;

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

/// `SHAddToRecentDocs(SHARD_PATHW, path)` (RNotepad's call after open/save)
/// must record-and-return, never stop the session.
#[test]
fn test_sh_add_to_recent_docs_pathw_returns_without_error() {
    use crate::guest_string::write_utf16_c_string;
    let mut engine = test_engine();
    let mut state = winapi_state_default();
    let path_ptr = 0x3000;
    write_utf16_c_string(&mut engine, path_ptr, 64, "C:\\tmp\\x.txt").ok();
    // RCX = SHARD_PATHW (0x3), RDX = path pointer.
    write_regs(&mut engine, 0x3, path_ptr, 0, 0, STACK_TOP);
    let result = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        shell32::dispatch_shell32(&mut ctx, "SHAddToRecentDocs")
    }
    .expect("dispatch failed")
    .expect("handler not found");
    assert_eq!(result.return_value, 0, "void return");
}

/// `SHGetFolderPathW` must return paths that point INTO the seeded default
/// skeleton: after a temp-root bottle is seeded, every CSIDL folder the
/// handler knows exists on the host under `{root}/drive_c/…`.
#[test]
fn test_sh_get_folder_path_w_returns_seeded_skeleton_paths() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed a fresh temp-root bottle so the returned folders exist on disk.
    let root = std::env::temp_dir().join(format!("wie-shfold-{}", std::process::id()));
    let _unused = std::fs::remove_dir_all(&root);
    crate::vfs::seed_default_skeleton(&root).expect("seed skeleton");
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(root.clone()),
        drive_d_root: None,
    };

    // csidl → the seeded guest folder (see handle_sh_get_folder_path_w).
    let cases: &[(u64, &str)] = &[
        (0x00, r"C:\Users\WIE\Desktop"),          // CSIDL_DESKTOP
        (0x05, r"C:\Users\WIE\Documents"),        // CSIDL_PERSONAL
        (0x1a, r"C:\Users\WIE\AppData\Roaming"),  // CSIDL_APPDATA
        (0x1c, r"C:\Users\WIE\AppData\Local"),    // CSIDL_LOCAL_APPDATA
        (0x23, r"C:\ProgramData"),                // CSIDL_COMMON_APPDATA
        (0x24, r"C:\Windows"),                    // CSIDL_WINDOWS
        (0x25, r"C:\Windows\System32"),           // CSIDL_SYSTEM
        (0x26, r"C:\Program Files"),              // CSIDL_PROGRAM_FILES
        (0x2a, r"C:\Program Files\Common Files"), // CSIDL_PROGRAM_FILES_COMMON
        (0x28, r"C:\Users\WIE"),                  // CSIDL_PROFILE
    ];
    for &(csidl, expected) in cases {
        // 5th arg (path buffer) sits at [rsp+0x28] in the Win64 ABI.
        let path_ptr = 0x6000_u64;
        engine
            .mem_write(STACK_TOP + 0x28, &path_ptr.to_le_bytes())
            .expect("write path_ptr stack slot");
        write_regs(&mut engine, 0, csidl, 0, 0, STACK_TOP);
        let result = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            shell32::dispatch_shell32(&mut ctx, "SHGetFolderPathW")
        }
        .expect("dispatch failed")
        .expect("handler not found");
        assert_eq!(result.return_value, 0, "S_OK for csidl {csidl:#x}");
        let returned = read_guest_utf16_raw(&mut engine, path_ptr, 260);
        assert_eq!(
            returned, expected,
            "csidl {csidl:#x} must return the seeded dir"
        );
        // The returned path maps into the seeded skeleton and exists on disk.
        let map = crate::vfs::guest_path_to_host(&state.file_io.volumes, &returned)
            .unwrap_or_else(|| panic!("csidl {csidl:#x}: returned path must map into the bottle"));
        assert!(
            map.host.is_dir(),
            "csidl {csidl:#x}: {} must exist on the host",
            map.host.display()
        );
    }
    let _unused = std::fs::remove_dir_all(&root);
}
