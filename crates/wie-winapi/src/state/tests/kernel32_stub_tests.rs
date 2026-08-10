//! Tests for the kernel32 stub handlers flagged by the audit: constant-return
//! no-ops (IsDebuggerPresent, DebugBreak, OutputDebugString*), the stateful
//! SetErrorMode, the identity/name writers (GetComputerNameW, GetUserNameW,
//! GetShortPathNameW, GetCompressedFileSizeW) and the handle-checking
//! GetThreadPriority. Each test pins the REAL handler behaviour as written,
//! not the audit's description.
//!
//! The second wave adds the A-variants (GetShortPathNameA, GetLongPathNameA),
//! the identity dir/process-name writers (GetUserProfileDirectory*,
//! QueryFullProcessImageName*), the file-io failure stubs (CreateHardLinkW,
//! FindFirst/NextStreamW, DeviceIoControl, OpenFileMapping, MoveFileWithProgressW),
//! the fake volume writers (GetVolumeInformation*, GetDiskFreeSpaceW,
//! GetLogicalDriveStringsW), the handle-object ops (DuplicateHandle, OpenThread,
//! TerminateThread, Suspend/ResumeThread, CreateJobObjectA, SignalObjectAndWait),
//! the open-file Backup* trio, and the SEH entry points (RaiseException,
//! RtlCaptureContext, RtlUnwindEx).
use super::*;

/// Dispatch one kernel32 API through the extra-dispatch table and return the
/// handler's return value (the guest RAX after the Win64 return).
fn dispatch_extra(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    kernel32::dispatch_kernel32_extra(&mut ctx, name)
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

#[test]
fn test_is_debugger_present_returns_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "IsDebuggerPresent"),
        0,
        "no debugger attached → FALSE"
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_debug_break_is_a_noop() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    // Emits a trace warning; must not fault or stop the session.
    assert_eq!(dispatch_extra(&mut engine, &mut state, "DebugBreak"), 0);
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_output_debug_string_a_returns_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // NULL message: the guest-memory read is skipped, call still succeeds.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "OutputDebugStringA"),
        1
    );
    // A real guest ANSI string is traced; the call still succeeds.
    let msg_va = 0x5000_u64;
    write_guest_ansi(&mut engine, msg_va, "hello from the guest");
    write_regs(&mut engine, msg_va, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "OutputDebugStringA"),
        1
    );
}

#[test]
fn test_output_debug_string_w_returns_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // NULL message: the guest-memory read is skipped, call still succeeds.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "OutputDebugStringW"),
        1
    );
    // A real guest UTF-16 string is traced; the call still succeeds.
    let msg_va = 0x5000_u64;
    write_guest_utf16(&mut engine, msg_va, "hello from the guest");
    write_regs(&mut engine, msg_va, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "OutputDebugStringW"),
        1
    );
}

#[test]
fn test_set_error_mode_stores_and_returns_previous() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    assert_eq!(state.process.error_mode, 0, "fresh state starts unset");
    // First call: no previous mode → returns 0, stores SEM_FAILCRITICALERRORS (1).
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_eq!(dispatch_extra(&mut engine, &mut state, "SetErrorMode"), 0);
    // Second call returns the previous mode and stores the new one.
    write_regs(&mut engine, 3, 0, 0, 0, 0);
    assert_eq!(dispatch_extra(&mut engine, &mut state, "SetErrorMode"), 1);
    assert_eq!(state.process.error_mode, 3);
}

#[test]
fn test_get_computer_name_w_writes_buffer_and_size() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    write_regs(&mut engine, name_va, size_va, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetComputerNameW");
    assert_eq!(returned, 1, "success returns nonzero");
    assert_eq!(state.process.last_error, 0);
    let name = read_guest_utf16_raw(&mut engine, name_va, 256);
    assert!(!name.is_empty(), "a computer name must be written");
    let written = read_test_i32(&mut engine, size_va);
    assert_eq!(
        written,
        i32::try_from(name.encode_utf16().count()).unwrap_or(0),
        "the size field holds the character count excluding NUL"
    );
}

#[test]
fn test_get_computer_name_w_null_buffer_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    // NULL name pointer → ERROR_INVALID_PARAMETER, return 0.
    write_regs(&mut engine, 0, size_va, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "GetComputerNameW"),
        0
    );
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

#[test]
fn test_get_user_name_w_writes_buffer_and_size() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let name_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    write_regs(&mut engine, name_va, size_va, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetUserNameW");
    assert_eq!(returned, 1, "success returns nonzero");
    assert_eq!(state.process.last_error, 0);
    let name = read_guest_utf16_raw(&mut engine, name_va, 256);
    assert!(!name.is_empty(), "a user name must be written");
    let written = read_test_i32(&mut engine, size_va);
    assert_eq!(
        written,
        i32::try_from(name.encode_utf16().count()).unwrap_or(0),
        "the size field holds the character count excluding NUL"
    );
}

#[test]
fn test_get_user_name_w_null_size_pointer_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // NULL size pointer → ERROR_INVALID_PARAMETER, return 0.
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);
    assert_eq!(dispatch_extra(&mut engine, &mut state, "GetUserNameW"), 0);
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

#[test]
fn test_get_short_path_name_w_returns_input_unchanged() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let src = 0x5000_u64;
    let dst = 0x6000_u64;
    let path = "C:\\Program Files\\App\\tool.exe";
    write_guest_utf16(&mut engine, src, path);
    // dst_len (r8) is in WCHARs; 64 covers the 32-char path plus NUL.
    write_regs(&mut engine, src, dst, 64, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetShortPathNameW");
    assert_eq!(
        returned,
        u64::try_from(path.encode_utf16().count()).unwrap_or(0),
        "return value is the character count excluding NUL"
    );
    let out = read_guest_utf16_raw(&mut engine, dst, 64);
    assert_eq!(out, path, "the mock short-name path echoes the input");
}

#[test]
fn test_get_compressed_file_size_w_missing_file() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_va = 0x5000_u64;
    write_guest_utf16(&mut engine, path_va, "C:\\no_such_file.bin");
    write_regs(&mut engine, path_va, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetCompressedFileSizeW");
    assert_eq!(
        returned, 0xffff_ffff,
        "an unresolvable path returns INVALID_FILE_ATTRIBUTES"
    );
    assert_eq!(state.process.last_error, 2, "ERROR_FILE_NOT_FOUND");
}

#[test]
fn test_get_compressed_file_size_w_virtual_file_size() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A virtual file resolves through the VFS without needing a bottle.
    state.file_io.virtual_files.push(crate::VirtualGuestFile {
        guest_path: "C:\\data.bin".to_owned(),
        bytes: vec![0_u8; 4096],
    });
    let path_va = 0x5000_u64;
    write_guest_utf16(&mut engine, path_va, "C:\\data.bin");
    write_regs(&mut engine, path_va, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetCompressedFileSizeW");
    assert_eq!(returned, 4096, "a resolved file reports its byte size");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_get_thread_priority_current_pseudo_handle_is_normal() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // (HANDLE)-2 = CURRENT_THREAD pseudohandle.
    write_regs(&mut engine, u64::MAX - 1, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetThreadPriority");
    assert_eq!(returned, 0, "THREAD_PRIORITY_NORMAL");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_get_thread_priority_invalid_handle_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234_5678, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetThreadPriority");
    assert_eq!(returned, 0x7fff_ffff, "THREAD_PRIORITY_ERROR_RETURN");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

#[test]
fn test_get_thread_priority_registered_thread_is_normal() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A real kernel thread object (detached: no guest CPU slot needed).
    let (handle, _thread) = state.kernel.sync.register_detached_thread(0x1111);
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetThreadPriority");
    assert_eq!(returned, 0, "THREAD_PRIORITY_NORMAL");
    assert_eq!(state.process.last_error, 0);
}

// ── Path name writers (A-variants + the long-path mirrors) ──────────────

#[test]
fn test_get_short_path_name_a_returns_input_unchanged() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let src = 0x5000_u64;
    let dst = 0x6000_u64;
    let path = r"C:\foo\bar.txt";
    write_guest_ansi(&mut engine, src, path);
    write_regs(&mut engine, src, dst, 64, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetShortPathNameA");
    assert_eq!(
        returned,
        u64::try_from(path.len()).unwrap_or(0),
        "return value is the byte count excluding NUL"
    );
    assert_eq!(
        read_guest_ansi_raw(&mut engine, dst, 64),
        path,
        "the mock short-name path echoes the input"
    );
}

#[test]
fn test_get_long_path_name_w_returns_input_unchanged() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let src = 0x5000_u64;
    let dst = 0x6000_u64;
    let path = r"C:\Users\test\file.txt";
    write_guest_utf16(&mut engine, src, path);
    write_regs(&mut engine, src, dst, 64, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetLongPathNameW");
    assert_eq!(
        returned,
        u64::try_from(path.encode_utf16().count()).unwrap_or(0),
        "return value is the character count excluding NUL"
    );
    assert_eq!(
        read_guest_utf16_raw(&mut engine, dst, 64),
        path,
        "the mock long-path name echoes the input"
    );
}

#[test]
fn test_get_long_path_name_a_returns_input_unchanged() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let src = 0x5000_u64;
    let dst = 0x6000_u64;
    let path = r"C:\Users\test\file.txt";
    write_guest_ansi(&mut engine, src, path);
    write_regs(&mut engine, src, dst, 64, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetLongPathNameA");
    assert_eq!(
        returned,
        u64::try_from(path.len()).unwrap_or(0),
        "return value is the byte count excluding NUL"
    );
    assert_eq!(
        read_guest_ansi_raw(&mut engine, dst, 64),
        path,
        "the mock long-path name echoes the input"
    );
}

// ── Identity dir / process-name writers ───────────────────────────────

#[test]
fn test_get_user_profile_directory_w_writes_default_profile() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let dir_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write dir capacity");
    write_regs(&mut engine, 0, dir_va, size_va, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetUserProfileDirectoryW");
    assert_eq!(returned, 1, "success returns nonzero");
    assert_eq!(state.process.last_error, 0);
    let dir = read_guest_utf16_raw(&mut engine, dir_va, 256);
    assert_eq!(dir, r"C:\Users\User", "no bottle → the default profile dir");
    assert_eq!(
        read_test_i32(&mut engine, size_va),
        i32::try_from(r"C:\Users\User".encode_utf16().count()).unwrap_or(0),
        "size field holds the character count excluding NUL"
    );
}

#[test]
fn test_get_user_profile_directory_a_writes_default_profile() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let dir_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write dir capacity");
    write_regs(&mut engine, 0, dir_va, size_va, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetUserProfileDirectoryA");
    assert_eq!(returned, 1, "success returns nonzero");
    let dir = read_guest_ansi_raw(&mut engine, dir_va, 256);
    assert_eq!(dir, r"C:\Users\User", "no bottle → the default profile dir");
    assert_eq!(
        read_test_i32(&mut engine, size_va),
        i32::try_from(r"C:\Users\User".len()).unwrap_or(0),
        "size field holds the byte count excluding NUL"
    );
}

#[test]
fn test_query_full_process_image_name_w_writes_main_module_path() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.main_module_path = r"C:\app.exe".to_owned();
    let name_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    write_regs(&mut engine, 0, 0, name_va, size_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "QueryFullProcessImageNameW");
    assert_eq!(returned, 1, "success returns nonzero");
    assert_eq!(state.process.last_error, 0);
    assert_eq!(
        read_guest_utf16_raw(&mut engine, name_va, 256),
        r"C:\app.exe"
    );
    assert_eq!(
        read_test_i32(&mut engine, size_va),
        i32::try_from(r"C:\app.exe".encode_utf16().count()).unwrap_or(0)
    );
}

#[test]
fn test_query_full_process_image_name_a_writes_main_module_path() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.main_module_path = r"C:\app.exe".to_owned();
    let name_va = 0x5000_u64;
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    write_regs(&mut engine, 0, 0, name_va, size_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "QueryFullProcessImageNameA");
    assert_eq!(returned, 1, "success returns nonzero");
    assert_eq!(
        read_guest_ansi_raw(&mut engine, name_va, 256),
        r"C:\app.exe"
    );
    assert_eq!(
        read_test_i32(&mut engine, size_va),
        i32::try_from(r"C:\app.exe".len()).unwrap_or(0)
    );
}

#[test]
fn test_query_full_process_image_name_w_null_buffer_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let size_va = 0x5100_u64;
    engine
        .mem_write(size_va, &256_u32.to_le_bytes())
        .expect("write name capacity");
    // NULL name pointer → ERROR_INVALID_PARAMETER, return 0.
    write_regs(&mut engine, 0, 0, 0, size_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "QueryFullProcessImageNameW");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 87, "ERROR_INVALID_PARAMETER");
}

// ── File-io failure stubs ─────────────────────────────────────────────

#[test]
fn test_create_hard_link_w_unsupported() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x5000, 0x5100, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "CreateHardLinkW");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 1, "ERROR_INVALID_FUNCTION");
}

#[test]
fn test_move_file_with_progress_w_delegates_to_move_file() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // NULL source → MoveFileW's empty-path guard → ERROR_PATH_NOT_FOUND.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "MoveFileWithProgressW");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 3, "ERROR_PATH_NOT_FOUND");

    // A real rename attempt under an explicit (nonexistent) bottle: the source
    // does not exist, so the host rename fails with ERROR_ACCESS_DENIED. The
    // explicit root keeps the probe out of the global app-data bottle.
    let bottle = std::env::temp_dir().join(format!("wie-move-no-bottle-{}", std::process::id()));
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(bottle),
        drive_d_root: None,
    };
    let src = 0x5000_u64;
    let dst = 0x5100_u64;
    write_guest_utf16(&mut engine, src, r"C:\a.txt");
    write_guest_utf16(&mut engine, dst, r"C:\b.txt");
    write_regs(&mut engine, src, dst, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "MoveFileWithProgressW");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 5, "ERROR_ACCESS_DENIED");
}

#[test]
fn test_find_first_stream_w_reports_no_streams() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x5000, 0x5100, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "FindFirstStreamW");
    assert_eq!(returned, u64::MAX, "INVALID_HANDLE_VALUE");
    assert_eq!(state.process.last_error, 38, "ERROR_HANDLE_EOF");
}

#[test]
fn test_find_next_stream_w_reports_no_more_streams() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x8000_0001, 0x5100, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "FindNextStreamW");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 38, "ERROR_HANDLE_EOF");
}

#[test]
fn test_device_io_control_unsupported() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x8000_0001, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "DeviceIoControl");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 1, "ERROR_INVALID_FUNCTION");
}

#[test]
fn test_open_file_mapping_w_and_a_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let w = dispatch_extra(&mut engine, &mut state, "OpenFileMappingW");
    let a = dispatch_extra(&mut engine, &mut state, "OpenFileMappingA");
    assert_eq!(w, 0, "W variant → NULL");
    assert_eq!(a, 0, "A variant → NULL");
    assert_eq!(state.process.last_error, 2, "ERROR_FILE_NOT_FOUND");
}

// ── Fake volume writers ───────────────────────────────────────────────

#[test]
fn test_set_file_attributes_w_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_va = 0x5000_u64;
    write_guest_utf16(&mut engine, path_va, r"C:\some.txt");
    // FILE_ATTRIBUTE_ARCHIVE
    write_regs(&mut engine, path_va, 0x20, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "SetFileAttributesW");
    assert_eq!(returned, 1, "best-effort success");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_set_file_time_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x8000_0001, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "SetFileTime");
    assert_eq!(returned, 1, "best-effort success");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_get_disk_free_space_w_writes_fake_values() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let spc_va = 0x5000_u64;
    let bps_va = 0x5004_u64;
    let free_va = 0x5008_u64;
    let total_va = 0x500c_u64;
    engine
        .mem_write(STACK_TOP + 0x28, &total_va.to_le_bytes())
        .expect("write total-clusters pointer");
    write_regs(&mut engine, 0, spc_va, bps_va, free_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetDiskFreeSpaceW");
    assert_eq!(returned, 1, "TRUE");
    assert_eq!(read_test_i32(&mut engine, spc_va), 8, "sectors per cluster");
    assert_eq!(read_test_i32(&mut engine, bps_va), 512, "bytes per sector");
    assert_eq!(
        read_test_i32(&mut engine, free_va),
        13_107_200,
        "half of FAKE_DISK_CLUSTERS"
    );
    assert_eq!(
        read_test_i32(&mut engine, total_va),
        26_214_400,
        "FAKE_DISK_CLUSTERS"
    );
}

#[test]
fn test_get_logical_drive_strings_w_writes_c_drive() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let buffer = 0x5000_u64;
    write_regs(&mut engine, 4, buffer, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetLogicalDriveStringsW");
    assert_eq!(returned, 4, "TCHAR count of the list");
    assert_eq!(read_guest_utf16_raw(&mut engine, buffer, 4), r"C:\");
    // A NULL / too-small buffer still reports the required TCHAR count.
    write_regs(&mut engine, 4, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetLogicalDriveStringsW");
    assert_eq!(returned, 4);
}

#[test]
fn test_get_volume_information_w_writes_volume_info() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let vol_name = 0x5000_u64;
    let serial_va = 0x5100_u64;
    let max_comp_va = 0x5200_u64;
    let flags_va = 0x5204_u64;
    let fs_name = 0x5210_u64;
    let fs_len_va = 0x5220_u64;
    // The trailing outputs are POINTERS stored in the caller's stack arg
    // slots at rsp+0x28..0x40 — each points at a guest output buffer.
    engine
        .mem_write(STACK_TOP + 0x28, &max_comp_va.to_le_bytes())
        .expect("write max-comp pointer");
    engine
        .mem_write(STACK_TOP + 0x30, &flags_va.to_le_bytes())
        .expect("write flags pointer");
    engine
        .mem_write(STACK_TOP + 0x38, &fs_name.to_le_bytes())
        .expect("write fs-name pointer");
    engine
        .mem_write(STACK_TOP + 0x40, &fs_len_va.to_le_bytes())
        .expect("write fs-len pointer");
    write_regs(&mut engine, 0, vol_name, 32, serial_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetVolumeInformationW");
    assert_eq!(returned, 1, "TRUE");
    assert_eq!(state.process.last_error, 0);
    assert_eq!(
        read_guest_utf16_raw(&mut engine, vol_name, 32),
        "Bottle",
        "default label when no bottle root is configured"
    );
    assert_eq!(
        read_test_i32(&mut engine, serial_va),
        0x1234_abcd,
        "fake volume serial matches the file-info path"
    );
    assert_eq!(
        read_test_i32(&mut engine, max_comp_va),
        255,
        "NTFS max component"
    );
    assert_eq!(
        read_test_i32(&mut engine, flags_va),
        0x0004_000E,
        "BOTTLE_FS_FLAGS"
    );
    assert_eq!(
        read_guest_utf16_raw(&mut engine, fs_name, 8),
        "NTFS",
        "fs-name string keeps its NUL terminator"
    );
    assert_eq!(
        read_test_i32(&mut engine, fs_len_va),
        4,
        "nFileSystemNameSize holds the char count excluding NUL"
    );
    // The arg slots themselves are only read, never clobbered.
    let mut slot = [0_u8; 8];
    engine
        .mem_read(STACK_TOP + 0x40, &mut slot)
        .expect("read fs-len slot");
    assert_eq!(u64::from_le_bytes(slot), fs_len_va);
}

#[test]
fn test_get_volume_information_w_null_trailing_pointers() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // All optional outputs NULL (serial, max-comp, flags, fs-name, fs-size):
    // Windows allows this; the call must still succeed.
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .expect("zero max-comp slot");
    engine
        .mem_write(STACK_TOP + 0x30, &0_u64.to_le_bytes())
        .expect("zero flags slot");
    engine
        .mem_write(STACK_TOP + 0x38, &0_u64.to_le_bytes())
        .expect("zero fs-name slot");
    engine
        .mem_write(STACK_TOP + 0x40, &0_u64.to_le_bytes())
        .expect("zero fs-len slot");
    write_regs(&mut engine, 0, 0x5000, 32, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetVolumeInformationW");
    assert_eq!(returned, 1, "TRUE with NULL optional outputs");
    assert_eq!(state.process.last_error, 0);
    // Nothing was written into the (zeroed) arg slots.
    let mut slot = [0_u8; 8];
    engine
        .mem_read(STACK_TOP + 0x40, &mut slot)
        .expect("read fs-len slot");
    assert_eq!(u64::from_le_bytes(slot), 0);
}

#[test]
fn test_get_volume_information_a_writes_volume_info() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let vol_name = 0x5000_u64;
    let serial_va = 0x5100_u64;
    let max_comp_va = 0x5200_u64;
    let flags_va = 0x5204_u64;
    let fs_name = 0x5210_u64;
    let fs_len_va = 0x5220_u64;
    // Same pointer-through-slot convention as the W variant.
    engine
        .mem_write(STACK_TOP + 0x28, &max_comp_va.to_le_bytes())
        .expect("write max-comp pointer");
    engine
        .mem_write(STACK_TOP + 0x30, &flags_va.to_le_bytes())
        .expect("write flags pointer");
    engine
        .mem_write(STACK_TOP + 0x38, &fs_name.to_le_bytes())
        .expect("write fs-name pointer");
    engine
        .mem_write(STACK_TOP + 0x40, &fs_len_va.to_le_bytes())
        .expect("write fs-len pointer");
    write_regs(&mut engine, 0, vol_name, 32, serial_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetVolumeInformationA");
    assert_eq!(returned, 1, "TRUE");
    assert_eq!(
        read_guest_ansi_raw(&mut engine, vol_name, 32),
        "Bottle",
        "default label when no bottle root is configured"
    );
    assert_eq!(
        read_test_i32(&mut engine, serial_va),
        0x1234_abcd,
        "fake volume serial matches the file-info path"
    );
    assert_eq!(
        read_test_i32(&mut engine, max_comp_va),
        255,
        "NTFS max component"
    );
    assert_eq!(
        read_test_i32(&mut engine, flags_va),
        0x0004_000E,
        "BOTTLE_FS_FLAGS"
    );
    assert_eq!(
        read_guest_ansi_raw(&mut engine, fs_name, 8),
        "NTFS",
        "fs-name string keeps its NUL terminator"
    );
    assert_eq!(
        read_test_i32(&mut engine, fs_len_va),
        4,
        "nFileSystemNameSize holds the byte count excluding NUL"
    );
}

#[test]
fn test_get_volume_information_a_null_trailing_pointers() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // All optional outputs NULL: the call must still succeed.
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .expect("zero max-comp slot");
    engine
        .mem_write(STACK_TOP + 0x30, &0_u64.to_le_bytes())
        .expect("zero flags slot");
    engine
        .mem_write(STACK_TOP + 0x38, &0_u64.to_le_bytes())
        .expect("zero fs-name slot");
    engine
        .mem_write(STACK_TOP + 0x40, &0_u64.to_le_bytes())
        .expect("zero fs-len slot");
    write_regs(&mut engine, 0, 0x5000, 32, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "GetVolumeInformationA");
    assert_eq!(returned, 1, "TRUE with NULL optional outputs");
    assert_eq!(state.process.last_error, 0);
}

// ── Kernel handle-object ops ──────────────────────────────────────────

#[test]
fn test_duplicate_handle_pseudo_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let target_va = 0x5000_u64;
    // (HANDLE)-1 pseudohandle → resolves to a thread object for the current TID.
    write_regs(&mut engine, 0, u64::MAX, 0, target_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "DuplicateHandle");
    assert_eq!(returned, 1, "TRUE");
    let mut out = [0_u8; 8];
    engine
        .mem_read(target_va, &mut out)
        .expect("read dup handle");
    let dup = u64::from_le_bytes(out);
    assert_ne!(dup, 0, "a new handle must be written to lpTargetHandle");
    assert!(
        state.kernel.sync.object(dup).is_some(),
        "the duplicated handle resolves to a kernel object"
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_duplicate_handle_registered_thread() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (src_handle, _thread) = state.kernel.sync.register_detached_thread(0x2222);
    let target_va = 0x5000_u64;
    write_regs(&mut engine, 0, src_handle, 0, target_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "DuplicateHandle");
    assert_eq!(returned, 1, "TRUE");
    let mut out = [0_u8; 8];
    engine
        .mem_read(target_va, &mut out)
        .expect("read dup handle");
    let dup = u64::from_le_bytes(out);
    assert_ne!(dup, src_handle, "a fresh handle value");
    assert!(
        state.kernel.sync.object(dup).is_some(),
        "duplicate resolves to a kernel object"
    );
    assert!(
        state.kernel.sync.object(src_handle).is_some(),
        "the source handle stays live"
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_duplicate_handle_invalid_source_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0xDEAD_BEEF, 0, 0x5000, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "DuplicateHandle");
    assert_eq!(returned, 0, "FALSE");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

#[test]
fn test_open_thread_finds_existing_thread() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (handle, _thread) = state.kernel.sync.register_detached_thread(0x7777);
    write_regs(&mut engine, 0x1000, 0, 0x7777, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "OpenThread");
    assert_eq!(
        returned, handle,
        "an existing TID resolves to its registered handle"
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_terminate_thread_marks_finished() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (handle, _thread) = state.kernel.sync.register_detached_thread(0x5555);
    write_regs(&mut engine, handle, 9, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "TerminateThread");
    assert_eq!(returned, 1, "TRUE");
    let exit_code = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Thread(t)) => {
            t.exit_code.load(std::sync::atomic::Ordering::Acquire)
        }
        _ => 0,
    };
    assert_eq!(exit_code, 9, "the thread records its exit code");

    // Unknown handle → FALSE + ERROR_INVALID_HANDLE.
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "TerminateThread");
    assert_eq!(returned, 0);
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

#[test]
fn test_suspend_thread_counts_suspensions() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (handle, _thread) = state.kernel.sync.register_detached_thread(0x3333);
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "SuspendThread"),
        0,
        "first suspend returns the previous count 0"
    );
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "SuspendThread"),
        1,
        "second suspend returns the previous count 1"
    );
    assert_eq!(
        state.process.suspended_threads.get(&0x3333_u32).copied(),
        Some(2),
        "the per-thread count accumulates"
    );
}

#[test]
fn test_resume_thread_running_thread_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (handle, _thread) = state.kernel.sync.register_detached_thread(0x4444);
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(
        dispatch_extra(&mut engine, &mut state, "ResumeThread"),
        0,
        "a running thread's previous suspend count is 0"
    );
    assert_eq!(state.process.last_error, 0);

    // Unknown handle → (DWORD)-1 + ERROR_INVALID_HANDLE.
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "ResumeThread");
    assert_eq!(returned, 0xffff_ffff);
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

#[test]
fn test_create_job_object_a_allocates_distinct_handles() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let first = dispatch_extra(&mut engine, &mut state, "CreateJobObjectA");
    let second = dispatch_extra(&mut engine, &mut state, "CreateJobObjectA");
    assert_ne!(first, 0, "first job handle is non-zero");
    assert_ne!(second, 0, "second job handle is non-zero");
    assert_ne!(first, second, "handles advance per call");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_signal_object_and_wait_signals_and_returns() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (signal_handle, _signal) = state.kernel.sync.register_event(false, false);
    let (wait_handle, _wait) = state.kernel.sync.register_event(false, true);
    write_regs(&mut engine, signal_handle, wait_handle, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "SignalObjectAndWait");
    assert_eq!(returned, 0, "WAIT_OBJECT_0");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_signal_object_and_wait_invalid_wait_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (signal_handle, _signal) = state.kernel.sync.register_event(false, false);
    write_regs(&mut engine, signal_handle, 0xDEAD, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "SignalObjectAndWait");
    assert_eq!(returned, 0xffff_ffff, "WAIT_FAILED");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

// ── Open-file Backup* trio ────────────────────────────────────────────

#[test]
fn test_backup_read_copies_bytes_and_advances_cursor() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let handle =
        kernel32::allocate_open_file(&mut state, r"C:\backup.bin", b"0123456789".to_vec(), None)
            .expect("open file");
    let buf = 0x5000_u64;
    let bytes_read_va = 0x5100_u64;
    write_regs(&mut engine, handle, buf, 4, bytes_read_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "BackupRead");
    assert_eq!(returned, 1, "TRUE");
    let mut data = [0_u8; 4];
    engine.mem_read(buf, &mut data).expect("read buffer");
    assert_eq!(&data, b"0123", "file bytes copied into the guest buffer");
    assert_eq!(
        read_test_i32(&mut engine, bytes_read_va),
        4,
        "bytes-read count written"
    );
    let file = state.file_io.open_files.get(&handle).expect("open file");
    assert_eq!(file.cursor, 4, "the read cursor advances");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_backup_seek_moves_cursor() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let handle =
        kernel32::allocate_open_file(&mut state, r"C:\backup.bin", b"0123456789".to_vec(), None)
            .expect("open file");
    let lo_va = 0x5100_u64;
    write_regs(&mut engine, handle, 5, 0, lo_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "BackupSeek");
    assert_eq!(returned, 1, "TRUE");
    assert_eq!(
        read_test_i32(&mut engine, lo_va),
        5,
        "low 32 bits written back"
    );
    let file = state.file_io.open_files.get(&handle).expect("open file");
    assert_eq!(file.cursor, 5, "the seek lands on the requested offset");
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_backup_write_stores_bytes_and_advances_cursor() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let handle = kernel32::allocate_open_file(&mut state, r"C:\backup.bin", vec![0_u8; 8], None)
        .expect("open file");
    let buf = 0x5000_u64;
    let written_va = 0x5100_u64;
    write_guest_ansi(&mut engine, buf, "ABCD");
    write_regs(&mut engine, handle, buf, 4, written_va, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "BackupWrite");
    assert_eq!(returned, 1, "TRUE");
    assert_eq!(read_test_i32(&mut engine, written_va), 4, "written count");
    let file = state.file_io.open_files.get(&handle).expect("open file");
    assert_eq!(file.cursor, 4, "the write cursor advances");
    assert_eq!(
        file.bytes.get(..4),
        Some(&b"ABCD"[..]),
        "guest bytes land in the file buffer"
    );
    assert_eq!(state.process.last_error, 0);
}

#[test]
fn test_backup_write_invalid_handle_errors() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0x5000, 4, 0x5100, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "BackupWrite");
    assert_eq!(returned, 0, "FALSE");
    assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
}

// ── SEH entry points ──────────────────────────────────────────────────

#[test]
fn test_rtl_capture_context_writes_context() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let ctx_va = 0x5000_u64;
    write_regs(&mut engine, ctx_va, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "RtlCaptureContext");
    assert_eq!(returned, 0);
    // ContextFlags (CONTEXT_FULL | CONTEXT_XSTATE) at +0x30 pin the write.
    let mut flags = [0_u8; 4];
    engine
        .mem_read(ctx_va + 0x30, &mut flags)
        .expect("read context flags");
    assert_eq!(u32::from_le_bytes(flags), 0x0010_001F);
    // A NULL target is a no-op that still returns success.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "RtlCaptureContext");
    assert_eq!(returned, 0);
}

#[test]
fn test_rtl_unwind_ex_trivial_returns_value() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // target_frame=0 and target_ip=0 → trivial return of the value in R9.
    write_regs(&mut engine, 0, 0, 0, 0x1234, 0);
    let returned = dispatch_extra(&mut engine, &mut state, "RtlUnwindEx");
    assert_eq!(returned, 0x1234);
}

#[test]
fn test_raise_exception_unhandled_surfaces_error() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // An exception code with no registered SEH frame chain: the guest has no
    // handler, so the dispatch must surface an error rather than succeed.
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
    let result = kernel32::dispatch_kernel32_extra(&mut ctx, "RaiseException");
    assert!(
        result.is_err(),
        "an unhandled RaiseException must surface as an error"
    );
}
