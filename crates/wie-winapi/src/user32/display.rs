use super::{
    Context, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
    FAKE_MONITOR_HANDLE, HandlerContext, Result, WinApiHandlerResult, checked_field_address,
    write_guest_fixed_ansi, write_guest_fixed_utf16, write_guest_i32, write_guest_u32,
};

pub(crate) fn write_fake_monitor_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    monitor_info_ptr: u64,
) -> Result<()> {
    if monitor_info_ptr == 0 {
        return Ok(());
    }

    // MONITORINFO:
    // DWORD cbSize;    offset 0
    // RECT  rcMonitor; offset 4
    // RECT  rcWork;    offset 20
    // DWORD dwFlags;   offset 36
    //
    // MONITORINFOEXA/W has the same prefix plus device name after offset 40.
    write_guest_u32(engine, monitor_info_ptr, 40)?;

    // rcMonitor = { left: 0, top: 0, right: 1920, bottom: 1080 }
    // Matches GetDeviceCaps HORZRES/VERTRES and GetSystemMetrics SM_CXSCREEN/
    // SM_CYSCREEN (all report a 1920×1080 fake display).
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 4, "rcMonitor.left"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 8, "rcMonitor.top"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 12, "rcMonitor.right"),
        1920,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 16, "rcMonitor.bottom"),
        1080,
    )?;

    // rcWork = { left: 0, top: 0, right: 1920, bottom: 1040 } (1080 - 40 taskbar)
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 20, "rcWork.left"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 24, "rcWork.top"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 28, "rcWork.right"),
        1920,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 32, "rcWork.bottom"),
        1040,
    )?;

    // MONITORINFOF_PRIMARY
    write_guest_u32(
        engine,
        checked_field_address(monitor_info_ptr, 36, "dwFlags"),
        1,
    )?;

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
    let metric_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSystemMetrics")?;

    let return_value = fake_system_metric(metric_index);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSystemMetrics")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!MonitorFromWindow`.
pub fn handle_monitor_from_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for MonitorFromWindow")?;

    let _flags = engine
        .read_rdx()
        .context("failed to read RDX for MonitorFromWindow")?;

    let return_address = engine
        .return_from_win64_api(FAKE_MONITOR_HANDLE)
        .context("failed to return from MonitorFromWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_MONITOR_HANDLE,
    })
}
/// Handles dynamic `USER32.dll!GetMonitorInfoA`.
pub fn handle_get_monitor_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let monitor_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetMonitorInfoA")?;

    let monitor_info_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetMonitorInfoA")?;

    let success = monitor_handle == FAKE_MONITOR_HANDLE && monitor_info_ptr != 0;

    if success {
        write_fake_monitor_info(engine, monitor_info_ptr)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetMonitorInfoA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!GetMonitorInfoW`.
pub fn handle_get_monitor_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let monitor_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetMonitorInfoW")?;

    let monitor_info_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetMonitorInfoW")?;

    let success = monitor_handle == FAKE_MONITOR_HANDLE && monitor_info_ptr != 0;

    if success {
        write_fake_monitor_info(engine, monitor_info_ptr)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetMonitorInfoW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!MonitorFromRect`.
pub fn handle_monitor_from_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for MonitorFromRect")?;

    let _flags = engine
        .read_rdx()
        .context("failed to read RDX for MonitorFromRect")?;

    let return_address = engine
        .return_from_win64_api(FAKE_MONITOR_HANDLE)
        .context("failed to return from MonitorFromRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_MONITOR_HANDLE,
    })
}
/// Handles dynamic `USER32.dll!MonitorFromPoint`.
pub fn handle_monitor_from_point(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _point_low = engine
        .read_rcx()
        .context("failed to read RCX for MonitorFromPoint")?;

    let _flags = engine
        .read_rdx()
        .context("failed to read RDX for MonitorFromPoint")?;

    let return_address = engine
        .return_from_win64_api(FAKE_MONITOR_HANDLE)
        .context("failed to return from MonitorFromPoint")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_MONITOR_HANDLE,
    })
}
/// Handles dynamic `USER32.dll!EnumDisplayMonitors`.
pub fn handle_enum_display_monitors(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_context = engine
        .read_rcx()
        .context("failed to read RCX for EnumDisplayMonitors")?;

    let _clip_rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for EnumDisplayMonitors")?;

    let _callback_ptr = engine
        .read_r8()
        .context("failed to read R8 for EnumDisplayMonitors")?;

    let _callback_data = engine
        .read_r9()
        .context("failed to read R9 for EnumDisplayMonitors")?;

    // First-pass behavior:
    // report success, but do not call the callback yet.
    //
    // If Lunar Magic later depends on the callback being invoked, we will need
    // to emulate a Win64 callback call into guest code with:
    //   callback(fake_monitor, fake_hdc, rect_ptr, data)
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from EnumDisplayMonitors")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles dynamic `USER32.dll!EnumDisplayDevicesA`.
pub fn handle_enum_display_devices_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_name_ptr = engine
        .read_rcx()
        .context("failed to read RCX for EnumDisplayDevicesA")?;

    let device_index = engine
        .read_rdx()
        .context("failed to read RDX for EnumDisplayDevicesA")?;

    let display_device_ptr = engine
        .read_r8()
        .context("failed to read R8 for EnumDisplayDevicesA")?;

    let _flags = engine
        .read_r9()
        .context("failed to read R9 for EnumDisplayDevicesA")?;

    let success = device_index == 0 && display_device_ptr != 0;

    if success {
        // DISPLAY_DEVICEA:
        // DWORD cb;                 offset 0
        // CHAR  DeviceName[32];     offset 4
        // CHAR  DeviceString[128];  offset 36
        // DWORD StateFlags;         offset 164
        // CHAR  DeviceID[128];      offset 168
        // CHAR  DeviceKey[128];     offset 296
        write_guest_u32(engine, display_device_ptr, 424)?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 4, "DeviceName"),
            32,
            b"\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 36, "DeviceString"),
            128,
            b"Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_field_address(display_device_ptr, 164, "StateFlags"),
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 168, "DeviceID"),
            128,
            b"MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 296, "DeviceKey"),
            128,
            b"\\Registry\\Machine\\System\\CurrentControlSet\\Enum\\DISPLAY\\WIE",
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EnumDisplayDevicesA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!EnumDisplayDevicesW`.
pub fn handle_enum_display_devices_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_name_ptr = engine
        .read_rcx()
        .context("failed to read RCX for EnumDisplayDevicesW")?;

    let device_index = engine
        .read_rdx()
        .context("failed to read RDX for EnumDisplayDevicesW")?;

    let display_device_ptr = engine
        .read_r8()
        .context("failed to read R8 for EnumDisplayDevicesW")?;

    let _flags = engine
        .read_r9()
        .context("failed to read R9 for EnumDisplayDevicesW")?;

    let success = device_index == 0 && display_device_ptr != 0;

    if success {
        // DISPLAY_DEVICEW:
        // DWORD cb;                 offset 0
        // WCHAR DeviceName[32];     offset 4
        // WCHAR DeviceString[128];  offset 68
        // DWORD StateFlags;         offset 324
        // WCHAR DeviceID[128];      offset 328
        // WCHAR DeviceKey[128];     offset 584
        write_guest_u32(engine, display_device_ptr, 840)?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 4, "DeviceName"),
            32,
            "\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 68, "DeviceString"),
            128,
            "Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_field_address(display_device_ptr, 324, "StateFlags"),
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 328, "DeviceID"),
            128,
            "MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 584, "DeviceKey"),
            128,
            "\\Registry\\Machine\\System\\CurrentControlSet\\Enum\\DISPLAY\\WIE",
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EnumDisplayDevicesW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
    let metric_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSystemMetricsForDpi")?;

    let dpi = engine
        .read_rdx()
        .context("failed to read RDX for GetSystemMetricsForDpi")?;

    let base_value = fake_system_metric(metric_index);
    let return_value = scale_system_metric_for_dpi(base_value, dpi)?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSystemMetricsForDpi")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
