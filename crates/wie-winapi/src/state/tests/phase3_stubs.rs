//! Phase-3 stub-wave tests: the winmm/gdi32/imm32/setupapi/dbghelp/winhttp/
//! shell32/ole32 handlers added for the DOOM Retro boot (pixel-format
//! delegation, no-device paths, fake WinHTTP handles, PropVariantClear).
use super::*;

/// Dispatch one API through a module's string-dispatch table.
macro_rules! dispatch_named {
    ($fn:expr, $engine:expr, $state:expr, $name:expr) => {{
        let mut ctx = HandlerContext::new($engine, default_env(), $state);
        $fn(&mut ctx, $name)
            .expect("dispatch must succeed")
            .expect("handled")
            .return_value
    }};
}

#[test]
fn test_winmm_no_device_paths() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    // One fake wave-out device, zero capture devices.
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "waveOutGetNumDevs"
        ),
        1
    );
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "waveInGetNumDevs"
        ),
        0
    );
    // MIDI has no driver: every operation fails with MMSYSERR_NODRIVER (97).
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "midiOutGetDevCapsA"
        ),
        97
    );
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "midiStreamOpen"
        ),
        97
    );
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "waveInOpen"
        ),
        97
    );
    // Timer period toggles are accepted no-ops.
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "timeBeginPeriod"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "timeEndPeriod"
        ),
        0
    );
}

#[test]
fn test_winmm_wave_out_get_dev_caps_w_writes_struct() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let caps = 0x3000_u64;
    write_regs(&mut engine, 0, caps, 84, 0, 0);
    assert_eq!(
        dispatch_named!(
            winmm::dispatch_winmm_extra,
            &mut engine,
            &mut state,
            "waveOutGetDevCapsW"
        ),
        0
    );
    let mut b = [0_u8; 84];
    engine.mem_read(caps, &mut b).expect("caps");
    let channels = u16::from_le_bytes([b[12], b[13]]);
    assert_eq!(channels, 2, "stereo fake device");
    let name = String::from_utf16_lossy(
        b[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect::<Vec<u16>>()
            .as_slice(),
    );
    assert!(
        name.contains("WIE"),
        "device name identifies the fake: {name}"
    );
}

#[test]
fn test_gdi32_pixel_format_delegation_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // SetPixelFormat(1) then GetPixelFormat → 1 (same state as wgl*).
    write_regs(&mut engine, 0x1234, 1, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "SetPixelFormat"
        ),
        1
    );
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "GetPixelFormat"
        ),
        1
    );
    // ChoosePixelFormat returns the stub format id (1).
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "ChoosePixelFormat"
        ),
        1
    );
}

#[test]
fn test_gdi32_bitmap_and_gamma_stubs() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 64, 32, 1, 32, 0);
    let bitmap = dispatch_named!(
        gdi32::dispatch_gdi32_extra,
        &mut engine,
        &mut state,
        "CreateBitmap"
    );
    assert_ne!(bitmap, 0, "valid dimensions produce an HBITMAP");
    write_regs(&mut engine, 0, 0, 1, 32, 0);
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "CreateBitmap"
        ),
        0,
        "zero dimensions return NULL"
    );
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "SetDeviceGammaRamp"
        ),
        1
    );
    assert_eq!(
        dispatch_named!(
            gdi32::dispatch_gdi32_extra,
            &mut engine,
            &mut state,
            "GetDeviceGammaRamp"
        ),
        0,
        "no gamma state is kept"
    );
}

#[test]
fn test_imm32_no_ime() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            imm32::dispatch_imm32,
            &mut engine,
            &mut state,
            "ImmAssociateContext"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            imm32::dispatch_imm32,
            &mut engine,
            &mut state,
            "ImmGetCandidateListW"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            imm32::dispatch_imm32,
            &mut engine,
            &mut state,
            "ImmGetIMEFileNameA"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            imm32::dispatch_imm32,
            &mut engine,
            &mut state,
            "ImmNotifyIME"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            imm32::dispatch_imm32,
            &mut engine,
            &mut state,
            "ImmSetCompositionWindow"
        ),
        0
    );
}

#[test]
fn test_setupapi_no_devices() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    // CM_* devnode queries: CR_NO_SUCH_DEVNODE (0x0E).
    assert_eq!(
        dispatch_named!(
            setupapi::dispatch_setupapi,
            &mut engine,
            &mut state,
            "CM_Get_Device_IDA"
        ),
        0x0E
    );
    assert_eq!(
        dispatch_named!(
            setupapi::dispatch_setupapi,
            &mut engine,
            &mut state,
            "CM_Locate_DevNodeA"
        ),
        0x0E
    );
    // SetupDi* enumeration: FALSE + ERROR_NO_MORE_ITEMS (259).
    assert_eq!(
        dispatch_named!(
            setupapi::dispatch_setupapi,
            &mut engine,
            &mut state,
            "SetupDiEnumDeviceInterfaces"
        ),
        0
    );
    assert_eq!(state.process.last_error, 259);
    assert_eq!(
        dispatch_named!(
            setupapi::dispatch_setupapi,
            &mut engine,
            &mut state,
            "SetupDiGetDeviceInterfaceDetailA"
        ),
        0
    );
}

#[test]
fn test_dbghelp_stubs() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            dbghelp::dispatch_dbghelp,
            &mut engine,
            &mut state,
            "SymSetOptions"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            dbghelp::dispatch_dbghelp,
            &mut engine,
            &mut state,
            "SymFunctionTableAccess64"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            dbghelp::dispatch_dbghelp,
            &mut engine,
            &mut state,
            "SymGetModuleBase64"
        ),
        0,
        "no module contains address 0"
    );
    assert_eq!(
        dispatch_named!(
            dbghelp::dispatch_dbghelp,
            &mut engine,
            &mut state,
            "StackWalk64"
        ),
        0
    );
    assert_eq!(
        dispatch_named!(
            dbghelp::dispatch_dbghelp,
            &mut engine,
            &mut state,
            "MiniDumpWriteDump"
        ),
        0
    );
}

#[test]
fn test_winhttp_handle_lifecycle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let session = dispatch_named!(
        winhttp::dispatch_winhttp,
        &mut engine,
        &mut state,
        "WinHttpOpen"
    );
    assert_ne!(session, 0, "fake session handle");
    let request = dispatch_named!(
        winhttp::dispatch_winhttp,
        &mut engine,
        &mut state,
        "WinHttpOpenRequest"
    );
    assert_ne!(request, 0, "fake request handle");
    // Request I/O fails cleanly with ERROR_WINHTTP_CANNOT_CONNECT.
    write_regs(&mut engine, request, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            winhttp::dispatch_winhttp,
            &mut engine,
            &mut state,
            "WinHttpSendRequest"
        ),
        0
    );
    assert_eq!(state.process.last_error, 12029);
    // Closing a live handle succeeds; closing it again fails.
    write_regs(&mut engine, session, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            winhttp::dispatch_winhttp,
            &mut engine,
            &mut state,
            "WinHttpCloseHandle"
        ),
        1
    );
    write_regs(&mut engine, session, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            winhttp::dispatch_winhttp,
            &mut engine,
            &mut state,
            "WinHttpCloseHandle"
        ),
        0
    );
}

#[test]
fn test_ole32_prop_variant_clear_zeroes() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let variant = 0x3000_u64;
    engine
        .mem_write(variant, &0xA5_u8.to_le_bytes())
        .expect("seed bytes");
    write_regs(&mut engine, variant, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            ole32::dispatch_ole32,
            &mut engine,
            &mut state,
            "PropVariantClear"
        ),
        0 // S_OK
    );
    let mut b = [0_u8; 16];
    engine.mem_read(variant, &mut b).expect("read back");
    assert_eq!(b, [0_u8; 16], "PROPVARIANT is fully zeroed");
}

#[test]
fn test_shell_execute_a_missing_file_returns_err() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // ShellExecuteA(NULL, "open", "C:\\does_not_exist.exe", ...) → SE_ERR_FNF (2).
    write_regs(&mut engine, 0, 0, 0x5000, 0, 0);
    write_guest_ansi(&mut engine, 0x5000, r"C:\does_not_exist.exe");
    let result = dispatch_named!(
        shell32::dispatch_shell32,
        &mut engine,
        &mut state,
        "ShellExecuteA"
    );
    assert_eq!(result, 2, "SE_ERR_FNF — the file does not exist");

    // ShellExecuteExA on the same file: FALSE with hInstApp = SE_ERR_FNF.
    let info = 0x6000_u64;
    write_guest_ansi(&mut engine, 0x6100, r"C:\does_not_exist.exe");
    engine
        .mem_write(info.wrapping_add(0x18), &0x6100_u64.to_le_bytes())
        .expect("lpFile");
    write_regs(&mut engine, info, 0, 0, 0, 0);
    assert_eq!(
        dispatch_named!(
            shell32::dispatch_shell32,
            &mut engine,
            &mut state,
            "ShellExecuteExA"
        ),
        0
    );
    let mut hinst = [0_u8; 8];
    engine
        .mem_read(info.wrapping_add(0x38), &mut hinst)
        .expect("hInstApp");
    assert_eq!(u64::from_le_bytes(hinst), 2, "hInstApp carries SE_ERR_FNF");
}
