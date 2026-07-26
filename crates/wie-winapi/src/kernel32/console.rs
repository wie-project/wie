use super::{
    Context, DEFAULT_CONSOLE_MODE_IN, DEFAULT_CONSOLE_MODE_OUT, FAKE_STDERR_HANDLE,
    FAKE_STDIN_HANDLE, FAKE_STDOUT_HANDLE, INVALID_HANDLE_VALUE, MAX_HOST_STDIN_LINE, Result,
    STD_ERROR_HANDLE_ID, STD_INPUT_HANDLE_ID, STD_OUTPUT_HANDLE_ID, WinApiHandlerResult,
    WinApiState, low_u32, ret_bool_true, ret_u64, write_guest_u32,
};

pub(crate) fn read_host_console_stdin_line() -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let mut out = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    let mut stdin = std::io::stdin().lock();
    loop {
        if out.len() >= MAX_HOST_STDIN_LINE {
            break;
        }
        if stdin.read(&mut byte)? == 0 {
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}
pub(crate) fn refill_stdin_from_host(state: &mut WinApiState) -> Result<bool, ()> {
    match read_host_console_stdin_line() {
        Ok(None) => Ok(false),
        Ok(Some(line)) => {
            state.file_io.stdin_bytes = line;
            state.file_io.stdin_cursor = 0;
            Ok(true)
        }
        Err(_) => Err(()),
    }
}
/// Handles `KERNEL32.dll!GetStdHandle`.
pub fn handle_get_std_handle(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let std_handle_id_raw = engine
        .read_rcx()
        .context("failed to read RCX for GetStdHandle")?;

    let std_handle_id = low_u32(std_handle_id_raw, "GetStdHandle id")?;

    let return_value = match std_handle_id {
        STD_INPUT_HANDLE_ID => FAKE_STDIN_HANDLE,
        STD_OUTPUT_HANDLE_ID => FAKE_STDOUT_HANDLE,
        STD_ERROR_HANDLE_ID => FAKE_STDERR_HANDLE,
        // Microsoft Learn: invalid standard device → INVALID_HANDLE_VALUE.
        _ => INVALID_HANDLE_VALUE,
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetStdHandle")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_set_console_ctrl_handler(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _handler = engine.read_rcx().context("SetConsoleCtrlHandler RCX")?;
    let _add = engine.read_rdx().context("SetConsoleCtrlHandler RDX")?;
    ret_bool_true(engine, "SetConsoleCtrlHandler")
}
pub fn handle_get_console_mode(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let handle = engine.read_rcx().context("GetConsoleMode RCX")?;
    let mode_ptr = engine.read_rdx().context("GetConsoleMode RDX")?;
    if mode_ptr == 0 {
        return ret_u64(engine, 0, "GetConsoleMode");
    }
    let mode = if handle == FAKE_STDIN_HANDLE {
        DEFAULT_CONSOLE_MODE_IN
    } else {
        DEFAULT_CONSOLE_MODE_OUT
    };
    write_guest_u32(engine, mode_ptr, mode)?;
    ret_bool_true(engine, "GetConsoleMode")
}
pub fn handle_set_console_mode(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _handle = engine.read_rcx().context("SetConsoleMode RCX")?;
    let _mode = engine.read_rdx().context("SetConsoleMode RDX")?;
    ret_bool_true(engine, "SetConsoleMode")
}
pub fn handle_get_console_screen_buffer_info(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _handle = engine
        .read_rcx()
        .context("GetConsoleScreenBufferInfo RCX")?;
    let info_ptr = engine
        .read_rdx()
        .context("GetConsoleScreenBufferInfo RDX")?;
    if info_ptr == 0 {
        return ret_u64(engine, 0, "GetConsoleScreenBufferInfo");
    }
    // CONSOLE_SCREEN_BUFFER_INFO is 22 bytes; pad to 24 so short stacks stay safe.
    // COORD dwSize {X,Y} at 0; COORD dwCursorPosition at 4; WORD wAttributes at 8;
    // SMALL_RECT srWindow at 10; COORD dwMaximumWindowSize at 18.
    let mut buf = [0_u8; 24];
    // dwSize = 80 x 25
    buf[0..2].copy_from_slice(&80_u16.to_le_bytes());
    buf[2..4].copy_from_slice(&25_u16.to_le_bytes());
    // wAttributes = 0x07 (gray on black)
    buf[8..10].copy_from_slice(&0x0007_u16.to_le_bytes());
    // srWindow: Left=0 Top=0 Right=79 Bottom=24
    buf[10..12].copy_from_slice(&0_u16.to_le_bytes());
    buf[12..14].copy_from_slice(&0_u16.to_le_bytes());
    buf[14..16].copy_from_slice(&79_u16.to_le_bytes());
    buf[16..18].copy_from_slice(&24_u16.to_le_bytes());
    // dwMaximumWindowSize
    buf[18..20].copy_from_slice(&80_u16.to_le_bytes());
    buf[20..22].copy_from_slice(&25_u16.to_le_bytes());
    engine
        .mem_write(info_ptr, &buf)
        .context("GetConsoleScreenBufferInfo write")?;
    ret_bool_true(engine, "GetConsoleScreenBufferInfo")
}
