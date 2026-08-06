use super::{
    ERROR_INVALID_PARAMETER, HandlerContext, OnceLock, Result, WinApiHandlerResult, WinApiState,
    write_guest_u32, write_mock_string_a, write_mock_string_w,
};

pub(crate) fn friendly_computer_name() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // Fast path: env vars (Windows, some Linux).
            if let Ok(name) = std::env::var("COMPUTERNAME") {
                return name;
            }
            // macOS: `scutil --get ComputerName`
            if let Ok(out) = std::process::Command::new("scutil")
                .args(["--get", "ComputerName"])
                .output()
                && out.status.success()
            {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if !s.is_empty() {
                    return s;
                }
            }
            // Fallback: `hostname` (any Unix).
            run_hostname().unwrap_or_else(|| "WIE-PC".to_owned())
        })
        .clone()
}
pub(crate) fn dns_hostname() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Ok(name) = std::env::var("HOSTNAME") {
                return name;
            }
            run_hostname().unwrap_or_else(|| "localhost".to_owned())
        })
        .clone()
}
pub(crate) fn run_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}
pub(crate) fn host_user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "User".to_owned())
}
pub(crate) fn write_name_to_buffer(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    name: &str,
    buf: u64,
    size_ptr: u64,
) -> Result<WinApiHandlerResult> {
    if buf == 0 || size_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_ptr, &mut size_buf)?;
    let buf_len = u64::from(u32::from_le_bytes(size_buf));
    let written = write_mock_string_w(engine, state, name, buf, buf_len)?;
    if written == 0 {
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    write_guest_u32(engine, size_ptr, u32::try_from(written).unwrap_or(0))?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn get_canonical_computer_name(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    buf: u64,
    size_ptr: u64,
) -> Result<WinApiHandlerResult> {
    write_name_to_buffer(engine, state, &friendly_computer_name(), buf, size_ptr)
}
pub(crate) fn get_dns_hostname(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    buf: u64,
    size_ptr: u64,
) -> Result<WinApiHandlerResult> {
    write_name_to_buffer(engine, state, &dns_hostname(), buf, size_ptr)
}
pub(crate) fn get_user_name_impl(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    buf: u64,
    size_ptr: u64,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    if buf == 0 || size_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_ptr, &mut size_buf)?;
    let buf_len = u64::from(u32::from_le_bytes(size_buf));
    let name = host_user_name();
    let written = if unicode {
        write_mock_string_w(engine, state, &name, buf, buf_len)?
    } else {
        write_mock_string_a(engine, state, &name, buf, buf_len)?
    };
    if written == 0 {
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    write_guest_u32(engine, size_ptr, u32::try_from(written).unwrap_or(0))?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn host_profile_dir(state: &WinApiState) -> String {
    if state.file_io.bottle_root.is_some() {
        let user = host_user_name();
        format!("C:\\Users\\{user}")
    } else {
        "C:\\Users\\User".to_owned()
    }
}
pub(crate) fn get_user_profile_dir_impl(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    buf: u64,
    size_ptr: u64,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    if buf == 0 || size_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_ptr, &mut size_buf)?;
    let buf_len = u64::from(u32::from_le_bytes(size_buf));
    let dir = host_profile_dir(state);
    let written = if unicode {
        write_mock_string_w(engine, state, &dir, buf, buf_len)?
    } else {
        write_mock_string_a(engine, state, &dir, buf, buf_len)?
    };
    if written == 0 {
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    write_guest_u32(engine, size_ptr, u32::try_from(written).unwrap_or(0))?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetComputerNameW` — friendly name (NetBIOS equivalent).
pub fn handle_get_computer_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buf = engine.read_rcx()?;
    let size_ptr = engine.read_rdx()?;
    get_canonical_computer_name(engine, state, buf, size_ptr)
}
/// Handles `KERNEL32.dll!GetComputerNameA` — friendly name (NetBIOS equivalent).
pub fn handle_get_computer_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // ANSI variant: write name to guest using ANSI encoding.
    let buf = engine.read_rcx()?;
    let size_ptr = engine.read_rdx()?;
    let r = get_canonical_computer_name(engine, state, buf, size_ptr)?;
    Ok(r)
}
/// Handles `KERNEL32.dll!GetComputerNameExW` — returns appropriate name type.
pub fn handle_get_computer_name_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name_type = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let size_ptr = engine.read_r8()?;
    match name_type {
        0 | 5 => get_canonical_computer_name(engine, state, buf, size_ptr),
        1 | 3 => get_dns_hostname(engine, state, buf, size_ptr),
        _ => {
            // Unsupported type → ERROR_INVALID_PARAMETER
            state.process.last_error = ERROR_INVALID_PARAMETER;
            ctx.finish(0)
        }
    }
}
/// Handles `KERNEL32.dll!GetUserNameW` — return real user name.
pub fn handle_get_user_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buf = engine.read_rcx()?;
    let size_ptr = engine.read_rdx()?;
    get_user_name_impl(engine, state, buf, size_ptr, true)
}
/// Handles `KERNEL32.dll!GetUserNameA` — return real user name.
pub fn handle_get_user_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buf = engine.read_rcx()?;
    let size_ptr = engine.read_rdx()?;
    get_user_name_impl(engine, state, buf, size_ptr, false)
}
/// Handles `KERNEL32.dll!QueryFullProcessImageNameW` — return main module path.
pub fn handle_query_full_process_image_name_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h_process = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let buf = engine.read_r8()?;
    let size_ptr = engine.read_r9()?;
    if buf == 0 || size_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_ptr, &mut size_buf)?;
    let buf_len = u64::from(u32::from_le_bytes(size_buf));
    let path = state.process.main_module_path.clone();
    let written = write_mock_string_w(engine, state, &path, buf, buf_len)?;
    if written == 0 {
        return ctx.finish(0);
    }
    let count = u32::try_from(written).unwrap_or(0);
    write_guest_u32(engine, size_ptr, count)?;
    state.process.last_error = 0;
    ctx.finish(1)
}
/// Handles `KERNEL32.dll!QueryFullProcessImageNameA` — return main module path.
pub fn handle_query_full_process_image_name_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h_process = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let buf = engine.read_r8()?;
    let size_ptr = engine.read_r9()?;
    if buf == 0 || size_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_ptr, &mut size_buf)?;
    let buf_len = u64::from(u32::from_le_bytes(size_buf));
    let path = state.process.main_module_path.clone();
    let written = write_mock_string_a(engine, state, &path, buf, buf_len)?;
    if written == 0 {
        return ctx.finish(0);
    }
    let count = u32::try_from(written).unwrap_or(0);
    write_guest_u32(engine, size_ptr, count)?;
    state.process.last_error = 0;
    ctx.finish(1)
}
