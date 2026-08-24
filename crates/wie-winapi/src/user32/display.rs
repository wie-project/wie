use super::{
    Context, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
    FAKE_DEVICE_CONTEXT_HANDLE, FAKE_MONITOR_HANDLE, HandlerContext, Result, WinApiHandlerResult,
    checked_address, write_guest_fixed_ansi, write_guest_fixed_utf16, write_guest_i32,
    write_guest_u32,
};
use crate::gdi32::{ArgReg, read_arg};

pub(crate) fn write_fake_monitor_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    monitor_info_va: u64,
) -> Result<()> {
    if monitor_info_va == 0 {
        return Ok(());
    }

    // MONITORINFO:
    // DWORD cbSize;    offset 0
    // RECT  rcMonitor; offset 4
    // RECT  rcWork;    offset 20
    // DWORD dwFlags;   offset 36
    //
    // MONITORINFOEXA/W has the same prefix plus device name after offset 40.
    write_guest_u32(engine, monitor_info_va, 40)?;

    // rcMonitor = { left: 0, top: 0, right: 1920, bottom: 1080 }
    // Matches GetDeviceCaps HORZRES/VERTRES and GetSystemMetrics SM_CXSCREEN/
    // SM_CYSCREEN (all report a 1920×1080 fake display).
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 4, "rcMonitor.left"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 8, "rcMonitor.top"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 12, "rcMonitor.right"),
        1920,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 16, "rcMonitor.bottom"),
        1080,
    )?;

    // rcWork = { left: 0, top: 0, right: 1920, bottom: 1040 } (1080 - 40 taskbar)
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 20, "rcWork.left"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 24, "rcWork.top"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 28, "rcWork.right"),
        1920,
    )?;
    write_guest_i32(
        engine,
        checked_address(monitor_info_va, 32, "rcWork.bottom"),
        1040,
    )?;

    // MONITORINFOF_PRIMARY
    write_guest_u32(engine, checked_address(monitor_info_va, 36, "dwFlags"), 1)?;

    Ok(())
}
pub(crate) fn fake_system_metric(metric_index: u64) -> u64 {
    // Standard SM_* values for a 1920×1080 32-bpp desktop (matches
    // GetDeviceCaps HORZRES/VERTRES). Identical return values are merged into
    // one arm (clippy match_same_arms); every metric whose real value is 0
    // (SM_DEBUG, SM_SWAPBUTTON, SM_CYKANJIWINDOW, SM_PENWINDOWS, SM_DBCSENABLED,
    // SM_SECURE, SM_CLEANBOOT, SM_SHOWSOUNDS, SM_SLOWMACHINE, SM_MIDEASTENABLED,
    // SM_MENUDROPALIGNMENT, SM_ARRANGE, SM_NETWORK, SM_XIMSCREEN, …) falls
    // through to `_ => 0`, matching Windows' behavior for invalid SM_* too.
    match metric_index {
        // SM_CXSCREEN / SM_CXFULLSCREEN / SM_CXMAXTRACK / SM_CYMAXTRACK /
        // SM_CXMAXIMIZED / SM_CXVIRTUALSCREEN / SM_CYVIRTUALSCREEN
        0 | 16 | 59 | 60 | 61 | 78 | 79 => 1920,

        // SM_CYSCREEN
        1 => 1080,

        // SM_CXVSCROLL / SM_CYHSCROLL / SM_CYVTHUMB / SM_CXHTHUMB /
        // SM_CYVSCROLL / SM_CXHSCROLL
        2 | 3 | 9 | 10 | 20 | 21 => 17,

        // SM_CYCAPTION
        4 => 23,

        // SM_CXBORDER / SM_CYBORDER / SM_MOUSEPRESENT / SM_MOUSEWHEELPRESENT /
        // SM_CMONITORS / SM_SAMEDISPLAYFORMAT
        5 | 6 | 19 | 75 | 80 | 81 => 1,

        // SM_CXDLGFRAME / SM_CYDLGFRAME (aliases SM_CXFIXEDFRAME /
        // SM_CYFIXEDFRAME) / SM_CXFRAME / SM_CYFRAME (aliases SM_CXSIZEFRAME /
        // SM_CYSIZEFRAME) / SM_CXDOUBLECLK / SM_CYDOUBLECLK / SM_CXDRAG /
        // SM_CYDRAG
        7 | 8 | 32 | 33 | 36 | 37 | 68 | 69 => 4,

        // SM_CXICON / SM_CYICON / SM_CXCURSOR / SM_CYCURSOR
        11..=14 => 32,

        // SM_CYMENU
        15 => 20,

        // SM_CYFULLSCREEN / SM_CYMAXIMIZED (1080 − 40 px taskbar)
        17 | 62 => 1040,

        // SM_CXMIN / SM_CXMINTRACK
        28 | 34 => 112,

        // SM_CYMIN / SM_CYMINTRACK / SM_CYMINIMIZED
        29 | 35 | 58 => 27,

        // SM_CXSIZE / SM_CYSIZE / SM_CYSMCAPTION / SM_CXMENUSIZE / SM_CYMENUSIZE
        30 | 31 | 51 | 54 | 55 => 18,

        // SM_CXICONSPACING / SM_CYICONSPACING
        38 | 39 => 75,

        // SM_CMOUSEBUTTONS (typical 5-button mouse)
        43 => 5,

        // SM_CXEDGE / SM_CYEDGE
        45 | 46 => 2,

        // SM_CXSMICON / SM_CYSMICON
        49 | 50 => 16,

        // SM_CXSMSIZE / SM_CYSMSIZE
        52 | 53 => 12,

        // SM_CXMENUCHECK / SM_CYMENUCHECK
        71 | 72 => 13,

        // SM_CXMINIMIZED
        57 => 160,

        // Zero-valued and unknown metrics.
        _ => 0,
    }
}
/// Handles `USER32.dll!GetSystemMetrics`.
pub fn handle_get_system_metrics(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let metric_index = read_arg(engine, ArgReg::Rcx, "GetSystemMetrics")?;

    let return_value = fake_system_metric(metric_index);

    ctx.finish(return_value)
}
/// Handles dynamic `USER32.dll!MonitorFromWindow`.
pub fn handle_monitor_from_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = read_arg(engine, ArgReg::Rcx, "MonitorFromWindow")?;

    let _flags = read_arg(engine, ArgReg::Rdx, "MonitorFromWindow")?;

    ctx.finish(FAKE_MONITOR_HANDLE)
}
/// Handles dynamic `USER32.dll!GetMonitorInfoA`.
pub fn handle_get_monitor_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let monitor_handle = read_arg(engine, ArgReg::Rcx, "GetMonitorInfoA")?;

    let monitor_info_va = read_arg(engine, ArgReg::Rdx, "GetMonitorInfoA")?;

    let success = monitor_handle == FAKE_MONITOR_HANDLE && monitor_info_va != 0;

    if success {
        write_fake_monitor_info(engine, monitor_info_va)?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles dynamic `USER32.dll!GetMonitorInfoW`.
pub fn handle_get_monitor_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let monitor_handle = read_arg(engine, ArgReg::Rcx, "GetMonitorInfoW")?;

    let monitor_info_va = read_arg(engine, ArgReg::Rdx, "GetMonitorInfoW")?;

    let success = monitor_handle == FAKE_MONITOR_HANDLE && monitor_info_va != 0;

    if success {
        write_fake_monitor_info(engine, monitor_info_va)?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles dynamic `USER32.dll!MonitorFromRect`.
pub fn handle_monitor_from_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _rect_va = read_arg(engine, ArgReg::Rcx, "MonitorFromRect")?;

    let _flags = read_arg(engine, ArgReg::Rdx, "MonitorFromRect")?;

    ctx.finish(FAKE_MONITOR_HANDLE)
}
/// Handles dynamic `USER32.dll!MonitorFromPoint`.
pub fn handle_monitor_from_point(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _point_low = read_arg(engine, ArgReg::Rcx, "MonitorFromPoint")?;

    let _flags = read_arg(engine, ArgReg::Rdx, "MonitorFromPoint")?;

    ctx.finish(FAKE_MONITOR_HANDLE)
}
/// Handles dynamic `USER32.dll!EnumDisplayMonitors`.
///
/// Reports one fake attached primary monitor by invoking the guest
/// `MONITORENUMPROC` callback exactly once, mirroring Windows: the callback
/// receives `(hMonitor, hdcMonitor, lprcMonitor, dwData)` and its BOOL return
/// becomes `EnumDisplayMonitors`'s own return (continue=TRUE, stop=FALSE).
///
/// SDL2's windows video driver relies on this: `WIN_InitModes` enumerates
/// displays via `EnumDisplayMonitors`, so if no callback is ever invoked the
/// driver sees zero displays, `WIN_InitModes` fails with "No displays
/// available", and `SDL_InitSubSystem(SDL_INIT_VIDEO)` bails — leaving
/// `SDL_GetNumVideoDisplays` to report "Video subsystem has not been
/// initialized".
pub fn handle_enum_display_monitors(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_context = read_arg(engine, ArgReg::Rcx, "EnumDisplayMonitors")?;

    let _clip_rect_va = read_arg(engine, ArgReg::Rdx, "EnumDisplayMonitors")?;

    let callback_va = read_arg(engine, ArgReg::R8, "EnumDisplayMonitors")?;

    let callback_data = read_arg(engine, ArgReg::R9, "EnumDisplayMonitors")?;

    if callback_va == 0 {
        // A NULL callback is invalid per Win32, but fail soft and report
        // success so no caller can crash.
        return ctx.finish(1);
    }

    let state = &mut *ctx.state;

    // The visible region of the fake monitor, as the MONITORENUMPROC
    // `lprcMonitor` argument (a RECT). SDL ignores it; other callers may read
    // it, so point it at the 1920×1080 desktop geometry the rest of the fake
    // surface reports rather than NULL.
    let rect_va = state.heap_state.heap.alloc_coherent(engine, 16);
    if rect_va != 0 {
        write_guest_i32(engine, checked_address(rect_va, 0, "lprcMonitor.left"), 0)?;
        write_guest_i32(engine, checked_address(rect_va, 4, "lprcMonitor.top"), 0)?;
        write_guest_i32(
            engine,
            checked_address(rect_va, 8, "lprcMonitor.right"),
            1920,
        )?;
        write_guest_i32(
            engine,
            checked_address(rect_va, 12, "lprcMonitor.bottom"),
            1080,
        )?;
    }

    // Pack for the WndProc bridge: RCX=hMonitor, RDX=hdcMonitor, R8=lprcMonitor,
    // R9=dwData; `Passthrough` forwards the callback's BOOL as the outer API's
    // return value.
    let request = super::GuestCallbackRequest {
        callback_address: callback_va,
        window_handle: FAKE_MONITOR_HANDLE,
        message: u32::try_from(FAKE_DEVICE_CONTEXT_HANDLE).unwrap_or(0),
        word_parameter: rect_va,
        long_parameter: callback_data,
        unicode: true,
        outer_return: super::OuterReturn::Passthrough,
    };
    Err(super::WinApiControlSignal::GuestCallbackRequested { request }.into())
}
/// Handles dynamic `USER32.dll!EnumDisplayDevicesA`.
pub fn handle_enum_display_devices_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_name_va = read_arg(engine, ArgReg::Rcx, "EnumDisplayDevicesA")?;

    let device_index = read_arg(engine, ArgReg::Rdx, "EnumDisplayDevicesA")?;

    let display_device_va = read_arg(engine, ArgReg::R8, "EnumDisplayDevicesA")?;

    let _flags = read_arg(engine, ArgReg::R9, "EnumDisplayDevicesA")?;

    let success = device_index == 0 && display_device_va != 0;

    if success {
        // DISPLAY_DEVICEA:
        // DWORD cb;                 offset 0
        // CHAR  DeviceName[32];     offset 4
        // CHAR  DeviceString[128];  offset 36
        // DWORD StateFlags;         offset 164
        // CHAR  DeviceID[128];      offset 168
        // CHAR  DeviceKey[128];     offset 296
        write_guest_u32(engine, display_device_va, 424)?;

        write_guest_fixed_ansi(
            engine,
            checked_address(display_device_va, 4, "DeviceName"),
            32,
            b"\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_address(display_device_va, 36, "DeviceString"),
            128,
            b"Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_address(display_device_va, 164, "StateFlags"),
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_address(display_device_va, 168, "DeviceID"),
            128,
            b"MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_address(display_device_va, 296, "DeviceKey"),
            128,
            b"\\Registry\\Machine\\System\\CurrentControlSet\\Enum\\DISPLAY\\WIE",
        )?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles dynamic `USER32.dll!EnumDisplayDevicesW`.
pub fn handle_enum_display_devices_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_name_va = read_arg(engine, ArgReg::Rcx, "EnumDisplayDevicesW")?;

    let device_index = read_arg(engine, ArgReg::Rdx, "EnumDisplayDevicesW")?;

    let display_device_va = read_arg(engine, ArgReg::R8, "EnumDisplayDevicesW")?;

    let _flags = read_arg(engine, ArgReg::R9, "EnumDisplayDevicesW")?;

    let success = device_index == 0 && display_device_va != 0;

    if success {
        // DISPLAY_DEVICEW:
        // DWORD cb;                 offset 0
        // WCHAR DeviceName[32];     offset 4
        // WCHAR DeviceString[128];  offset 68
        // DWORD StateFlags;         offset 324
        // WCHAR DeviceID[128];      offset 328
        // WCHAR DeviceKey[128];     offset 584
        write_guest_u32(engine, display_device_va, 840)?;

        write_guest_fixed_utf16(
            engine,
            checked_address(display_device_va, 4, "DeviceName"),
            32,
            "\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_address(display_device_va, 68, "DeviceString"),
            128,
            "Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_address(display_device_va, 324, "StateFlags"),
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_address(display_device_va, 328, "DeviceID"),
            128,
            "MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_address(display_device_va, 584, "DeviceKey"),
            128,
            "\\Registry\\Machine\\System\\CurrentControlSet\\Enum\\DISPLAY\\WIE",
        )?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
pub(crate) fn scale_system_metric_for_dpi(base_value: u64, dpi: u64) -> Result<u64> {
    if dpi == 0 {
        return Ok(base_value);
    }

    // Floor-scale: base * dpi / 96 (Win32 DPI convention).
    let product = base_value
        .checked_mul(dpi)
        .context("GetSystemMetricsForDpi multiplication overflow")?;
    Ok(product.checked_div(96).unwrap_or(0))
}
/// Handles dynamic `USER32.dll!GetSystemMetricsForDpi`.
pub fn handle_get_system_metrics_for_dpi(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let metric_index = read_arg(engine, ArgReg::Rcx, "GetSystemMetricsForDpi")?;

    let dpi = read_arg(engine, ArgReg::Rdx, "GetSystemMetricsForDpi")?;

    let base_value = fake_system_metric(metric_index);
    let return_value = scale_system_metric_for_dpi(base_value, dpi)?;

    ctx.finish(return_value)
}
