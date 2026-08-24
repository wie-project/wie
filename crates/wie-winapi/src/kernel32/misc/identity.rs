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
/// Write `name` into a guest `(buffer, &size)` pair.
///
/// Shared tail of every "name to caller buffer" API: NULL buffer/size fails
/// with `ERROR_INVALID_PARAMETER`; truncation returns 0 leaving the size slot
/// untouched; success writes back the written count and returns 1.
///
/// NOTE: this always writes UTF-16 units (`write_mock_string_w`). The ANSI
/// variants that forward here (`GetComputerNameA`, `GetUserNameA`,
/// `GetUserProfileDirectoryA`) therefore receive UTF-16 bytes where real
/// Windows writes ACP characters — flagged as a follow-up ticket, not
/// changed here (behavior-preserving refactor).
pub(crate) fn write_name_to_buffer(
    ctx: &mut HandlerContext<'_>,
    name: &str,
    name_va: u64,
    size_va: u64,
) -> Result<WinApiHandlerResult> {
    if name_va == 0 || size_va == 0 {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    ctx.engine.mem_read(size_va, &mut size_buf)?;
    let name_cap = u64::from(u32::from_le_bytes(size_buf));
    let written = write_mock_string_w(&mut *ctx.engine, &mut *ctx.state, name, name_va, name_cap)?;
    if written == 0 {
        return ctx.finish(0);
    }
    write_guest_u32(
        &mut *ctx.engine,
        size_va,
        u32::try_from(written).unwrap_or(0),
    )?;
    ctx.state.process.last_error = 0;
    ctx.finish(1)
}
pub(crate) fn get_canonical_computer_name(
    ctx: &mut HandlerContext<'_>,
    name_va: u64,
    size_va: u64,
) -> Result<WinApiHandlerResult> {
    write_name_to_buffer(ctx, &friendly_computer_name(), name_va, size_va)
}
pub(crate) fn get_dns_hostname(
    ctx: &mut HandlerContext<'_>,
    name_va: u64,
    size_va: u64,
) -> Result<WinApiHandlerResult> {
    write_name_to_buffer(ctx, &dns_hostname(), name_va, size_va)
}
/// Shared body of `GetUserNameW` / `GetUserNameA` (see the divergence note on
/// [`write_name_to_buffer`] for the A variant).
fn get_user_name_impl(
    ctx: &mut HandlerContext<'_>,
    name_va: u64,
    size_va: u64,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    if name_va == 0 || size_va == 0 {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    ctx.engine.mem_read(size_va, &mut size_buf)?;
    let name_cap = u64::from(u32::from_le_bytes(size_buf));
    let name = host_user_name();
    let written = if unicode {
        write_mock_string_w(&mut *ctx.engine, &mut *ctx.state, &name, name_va, name_cap)?
    } else {
        write_mock_string_a(&mut *ctx.engine, &mut *ctx.state, &name, name_va, name_cap)?
    };
    if written == 0 {
        return ctx.finish(0);
    }
    write_guest_u32(
        &mut *ctx.engine,
        size_va,
        u32::try_from(written).unwrap_or(0),
    )?;
    ctx.state.process.last_error = 0;
    ctx.finish(1)
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
    ctx: &mut HandlerContext<'_>,
    name_va: u64,
    size_va: u64,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    if name_va == 0 || size_va == 0 {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    ctx.engine.mem_read(size_va, &mut size_buf)?;
    let name_cap = u64::from(u32::from_le_bytes(size_buf));
    let dir = host_profile_dir(ctx.state);
    let written = if unicode {
        write_mock_string_w(&mut *ctx.engine, &mut *ctx.state, &dir, name_va, name_cap)?
    } else {
        write_mock_string_a(&mut *ctx.engine, &mut *ctx.state, &dir, name_va, name_cap)?
    };
    if written == 0 {
        return ctx.finish(0);
    }
    write_guest_u32(
        &mut *ctx.engine,
        size_va,
        u32::try_from(written).unwrap_or(0),
    )?;
    ctx.state.process.last_error = 0;
    ctx.finish(1)
}
/// Handles `KERNEL32.dll!GetComputerNameW` — friendly name (NetBIOS equivalent).
pub fn handle_get_computer_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let name_va = ctx.engine.read_rcx()?;
    let size_va = ctx.engine.read_rdx()?;
    get_canonical_computer_name(ctx, name_va, size_va)
}
/// Handles `KERNEL32.dll!GetComputerNameA` — forwarded to the W variant.
///
/// Follow-up ticket (behavior NOT changed here): the shared writer emits
/// UTF-16 units where real Windows would write ANSI bytes for A-callers.
pub fn handle_get_computer_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_computer_name_w(ctx)
}
/// Handles `KERNEL32.dll!GetComputerNameExW` — returns appropriate name type.
pub fn handle_get_computer_name_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name_type = engine.read_rcx()?;
    let name_va = engine.read_rdx()?;
    let size_va = engine.read_r8()?;
    match name_type {
        0 | 5 => get_canonical_computer_name(ctx, name_va, size_va),
        1 | 3 => get_dns_hostname(ctx, name_va, size_va),
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
    let name_va = engine.read_rcx()?;
    let size_va = engine.read_rdx()?;
    get_user_name_impl(ctx, name_va, size_va, true)
}
/// Handles `KERNEL32.dll!GetUserNameA` — return real user name.
pub fn handle_get_user_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let name_va = engine.read_rcx()?;
    let size_va = engine.read_rdx()?;
    get_user_name_impl(ctx, name_va, size_va, false)
}
/// Handles `KERNEL32.dll!QueryFullProcessImageNameW` — return main module path.
pub fn handle_query_full_process_image_name_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h_process = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let name_va = engine.read_r8()?;
    let size_va = engine.read_r9()?;
    if name_va == 0 || size_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_va, &mut size_buf)?;
    let name_cap = u64::from(u32::from_le_bytes(size_buf));
    let path = state.process.main_module_path.clone();
    let written = write_mock_string_w(engine, state, &path, name_va, name_cap)?;
    if written == 0 {
        return ctx.finish(0);
    }
    let count = u32::try_from(written).unwrap_or(0);
    write_guest_u32(engine, size_va, count)?;
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
    let name_va = engine.read_r8()?;
    let size_va = engine.read_r9()?;
    if name_va == 0 || size_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut size_buf = [0_u8; 4];
    engine.mem_read(size_va, &mut size_buf)?;
    let name_cap = u64::from(u32::from_le_bytes(size_buf));
    let path = state.process.main_module_path.clone();
    let written = write_mock_string_a(engine, state, &path, name_va, name_cap)?;
    if written == 0 {
        return ctx.finish(0);
    }
    let count = u32::try_from(written).unwrap_or(0);
    write_guest_u32(engine, size_va, count)?;
    state.process.last_error = 0;
    ctx.finish(1)
}
