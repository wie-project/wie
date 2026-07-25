use super::{
    Context, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
    FAKE_MONITOR_HANDLE, Result, WinApiHandlerResult, checked_field_address,
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

    // rcMonitor = { left: 0, top: 0, right: 1024, bottom: 768 }
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 4, "rcMonitor.left")?,
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 8, "rcMonitor.top")?,
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 12, "rcMonitor.right")?,
        1024,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 16, "rcMonitor.bottom")?,
        768,
    )?;

    // rcWork = { left: 0, top: 0, right: 1024, bottom: 728 }
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 20, "rcWork.left")?,
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 24, "rcWork.top")?,
        0,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 28, "rcWork.right")?,
        1024,
    )?;
    write_guest_i32(
        engine,
        checked_field_address(monitor_info_ptr, 32, "rcWork.bottom")?,
        728,
    )?;

    // MONITORINFOF_PRIMARY
    write_guest_u32(
        engine,
        checked_field_address(monitor_info_ptr, 36, "dwFlags")?,
        1,
    )?;

    Ok(())
}
pub(crate) fn fake_system_metric(metric_index: u64) -> u64 {
    match metric_index {
        // SM_CXSCREEN / SM_CXFULLSCREEN
        0 | 16 => 1024,

        // SM_CYSCREEN
        1 => 768,

        // SM_CXVSCROLL / SM_CYHSCROLL
        2 | 3 => 17,

        // SM_CYCAPTION
        4 => 23,

        // SM_CXBORDER / SM_CYBORDER / SM_MOUSEPRESENT / SM_CMONITORS
        5 | 6 | 19 | 80 => 1,

        // SM_CXDLGFRAME / SM_CYDLGFRAME / SM_CXFRAME / SM_CYFRAME /
        // SM_CXDOUBLECLK / SM_CYDOUBLECLK
        7 | 8 | 32 | 33 | 36 | 37 => 4,

        // SM_CXICON / SM_CYICON / SM_CXCURSOR / SM_CYCURSOR
        11..=14 => 32,

        // SM_CYMENU
        15 => 20,

        // SM_CYFULLSCREEN
        17 => 728,

        // SM_CXMIN / SM_CXMINTRACK
        28 | 34 => 112,

        // SM_CYMIN / SM_CYMINTRACK
        29 | 35 => 27,

        // SM_CXSIZE / SM_CYSIZE
        30 | 31 => 18,

        // SM_CXICONSPACING / SM_CYICONSPACING
        38 | 39 => 75,

        // Unknown and zero-valued metrics.
        _ => 0,
    }
}
pub fn handle_get_system_metrics(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_monitor_from_window(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_monitor_info_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_monitor_info_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_monitor_from_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_monitor_from_point(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_enum_display_monitors(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_enum_display_devices_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
            checked_field_address(display_device_ptr, 4, "DeviceName")?,
            32,
            b"\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 36, "DeviceString")?,
            128,
            b"Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_field_address(display_device_ptr, 164, "StateFlags")?,
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 168, "DeviceID")?,
            128,
            b"MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_ansi(
            engine,
            checked_field_address(display_device_ptr, 296, "DeviceKey")?,
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
pub fn handle_enum_display_devices_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
            checked_field_address(display_device_ptr, 4, "DeviceName")?,
            32,
            "\\\\.\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 68, "DeviceString")?,
            128,
            "Generic Display",
        )?;

        write_guest_u32(
            engine,
            checked_field_address(display_device_ptr, 324, "StateFlags")?,
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 328, "DeviceID")?,
            128,
            "MONITOR\\WIE\\DISPLAY1",
        )?;

        write_guest_fixed_utf16(
            engine,
            checked_field_address(display_device_ptr, 584, "DeviceKey")?,
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
pub fn handle_get_system_metrics_for_dpi(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
