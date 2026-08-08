//! Version API (version.dll) tests: GetFileVersionInfoSizeW / GetFileVersionInfoW / VerQueryValueW against a real file in a bottle.
use super::*;

// ── Version API (version.dll) ────────────────────────────────────────────

/// Full dispatch of the version flow against a real file in a bottle:
/// GetFileVersionInfoSizeW → GetFileVersionInfoW → VerQueryValueW. Uses the
/// windres-built micro as the target file (skips when the micro suite has not
/// been built, like the sibling micro tests).
#[test]
fn test_version_flow_reads_a_bottle_file() {
    use std::path::PathBuf;

    let mut src = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    src.pop();
    src.pop();
    src.push("micro-exes/out/version_query.exe");
    if !src.is_file() {
        return;
    }
    let exe_bytes = std::fs::read(&src).expect("read micro exe");

    // A scratch bottle holding the target at C:\App\sample.exe.
    let root = std::env::temp_dir().join(format!("wie-version-test-{}", std::process::id()));
    let _unused = std::fs::remove_dir_all(&root);
    let drive_c = root.join("drive_c").join("App");
    std::fs::create_dir_all(&drive_c).expect("create bottle drive_c/App");
    std::fs::write(drive_c.join("sample.exe"), &exe_bytes).expect("copy target into bottle");

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(root.clone()),
        drive_d_root: None,
    };
    state.file_io.current_directory_wide = r"C:\".encode_utf16().collect();
    state.process.main_module_file_name = "other.exe".to_owned();
    state.process.main_module_path = r"C:\other.exe".to_owned();

    // Guest path + handle/output buffers live in mapped memory.
    let path_va = 0x6000_u64;
    let handle_va = 0x7000_u64;
    let data_va = 0x8000_u64;
    let out_va = 0x9000_u64;
    let len_va = 0xA000_u64;
    let wide_path: Vec<u8> = r"C:\App\sample.exe"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .chain(0_u16.to_le_bytes())
        .collect();
    engine
        .mem_write(path_va, &wide_path)
        .expect("write guest path");

    // GetFileVersionInfoSizeW: the raw block length, written via the vfs.
    write_regs(&mut engine, path_va, handle_va, 0, 0, 0);
    let id = crate::resolve_winapi_id("version.dll", "GetFileVersionInfoSizeW")
        .expect("GetFileVersionInfoSizeW must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileVersionInfoSizeW must dispatch");
    let size = u32::try_from(r.return_value).expect("size fits u32");
    assert!(size > 52, "size covers the fixed info: {size}");

    // GetFileVersionInfoW: copies the raw block into the guest buffer.
    write_regs(&mut engine, path_va, 0, u64::from(size), data_va, 0);
    let id = crate::resolve_winapi_id("version.dll", "GetFileVersionInfoW")
        .expect("GetFileVersionInfoW must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileVersionInfoW must dispatch");
    assert_eq!(r.return_value, 1, "GetFileVersionInfoW succeeds");
    let mut block = vec![0_u8; usize::try_from(size).expect("size fits usize")];
    engine
        .mem_read(data_va, &mut block)
        .expect("read copied block");

    // VerQueryValueW("\"): the fixed info reports 1.2.3.4.
    let root_path: Vec<u8> = "\\"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .chain(0_u16.to_le_bytes())
        .collect();
    engine
        .mem_write(0x6000, &root_path)
        .expect("write root path");
    write_regs(&mut engine, data_va, 0x6000, out_va, len_va, 0);
    let id = crate::resolve_winapi_id("version.dll", "VerQueryValueW")
        .expect("VerQueryValueW must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("VerQueryValueW must dispatch");
    assert_eq!(r.return_value, 1, "root query succeeds");
    let mut fixed_ptr_bytes = [0_u8; 8];
    engine
        .mem_read(out_va, &mut fixed_ptr_bytes)
        .expect("read fixed info pointer");
    let fixed_va = u64::from_le_bytes(fixed_ptr_bytes);
    let mut file_version_ms = [0_u8; 4];
    engine
        .mem_read(fixed_va + 8, &mut file_version_ms)
        .expect("read dwFileVersionMS");
    let mut file_version_ls = [0_u8; 4];
    engine
        .mem_read(fixed_va + 12, &mut file_version_ls)
        .expect("read dwFileVersionLS");
    assert_eq!(u32::from_le_bytes(file_version_ms), 0x0001_0002);
    assert_eq!(u32::from_le_bytes(file_version_ls), 0x0003_0004);

    let _unused = std::fs::remove_dir_all(&root);
}

/// GetFileVersionInfoSizeW on a missing file fails with ERROR_FILE_NOT_FOUND
/// (the honest disposition, not a fabricated size).
#[test]
fn test_version_size_missing_file_sets_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(std::env::temp_dir()),
        drive_d_root: None,
    };
    state.file_io.current_directory_wide = r"C:\".encode_utf16().collect();
    let path_va = 0x6000_u64;
    let wide_path: Vec<u8> = r"C:\no-such-file.exe"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .chain(0_u16.to_le_bytes())
        .collect();
    engine
        .mem_write(path_va, &wide_path)
        .expect("write guest path");
    write_regs(&mut engine, path_va, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("version.dll", "GetFileVersionInfoSizeW")
        .expect("GetFileVersionInfoSizeW must resolve");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileVersionInfoSizeW must dispatch");
    assert_eq!(r.return_value, 0, "missing file yields size 0");
    assert_eq!(state.process.last_error, 2, "ERROR_FILE_NOT_FOUND");
}
