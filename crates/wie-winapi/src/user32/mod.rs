pub(crate) use crate::guest_memory::{
    checked_field_address, read_bytes as read_guest_bytes, read_i32 as read_guest_i32,
    read_u32 as read_guest_u32, read_u64 as read_guest_u64, write_bytes as write_guest_bytes,
    write_i32 as write_guest_i32, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
pub(crate) use crate::guest_string::{
    read_ansi_lossy as read_guest_ansi_lossy, read_utf16_lossy as read_guest_utf16_lossy,
    write_ansi_c_string as write_guest_ansi_c_string, write_fixed_ansi as write_guest_fixed_ansi,
    write_fixed_utf16 as write_guest_fixed_utf16,
    write_utf16_c_string as write_guest_utf16_c_string,
};

pub(crate) use crate::{
    GuestCallbackRequest, MessageQueueIdlePolicy, QueuedWindowMessage, TimerRecord,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WindowClassRecord, WindowRecord,
    WindowsHookRecord,
};
pub(crate) use anyhow::{Context, Result};

pub(crate) const FAKE_ICON_HANDLE: u64 = 0x0000_0000_6600_0001;
pub(crate) const FAKE_CURSOR_HANDLE: u64 = 0x0000_0000_6600_0002;
pub(crate) const IDOK: u64 = 1;

pub(crate) const WM_MDICREATE: u32 = 0x0220;
pub(crate) const FAKE_MONITOR_HANDLE: u64 = 0x0000_0000_6600_0010;
pub(crate) const DISPLAY_DEVICE_ATTACHED_TO_DESKTOP: u32 = 0x0000_0001;
pub(crate) const DISPLAY_DEVICE_PRIMARY_DEVICE: u32 = 0x0000_0004;
pub(crate) const FAKE_WINDOW_HANDLE: u64 = 0x0000_0000_6600_0100;
pub(crate) const FAKE_DEVICE_CONTEXT_HANDLE: u64 = 0x0000_0000_6600_0200;
pub(crate) const FAKE_IMAGE_HANDLE: u64 = 0x0000_0000_6600_0300;
pub(crate) const FAKE_DESKTOP_WINDOW_HANDLE: u64 = 0x0000_0000_6600_0110;
pub(crate) const FAKE_SYSTEM_COLOR_BRUSH_BASE: u64 = 0x0000_0000_6601_0000;
pub(crate) const FAKE_PROCESS_ID: u32 = 1;
pub(crate) const FAKE_THREAD_ID: u64 = 1;

pub(crate) const DIALOG_BASE_UNIT_X: u32 = 8;
pub(crate) const DIALOG_BASE_UNIT_Y: u32 = 16;

pub(crate) const WM_QUIT: u32 = 0x0012;

pub(crate) const WM_KEYDOWN: u32 = 0x0100;
pub(crate) const WM_KEYUP: u32 = 0x0101;
pub(crate) const WM_CHAR: u32 = 0x0102;
pub(crate) const WM_DEADCHAR: u32 = 0x0103;
pub(crate) const WM_SYSKEYDOWN: u32 = 0x0104;
pub(crate) const WM_SYSKEYUP: u32 = 0x0105;
pub(crate) const WM_SYSCHAR: u32 = 0x0106;
pub(crate) const WM_SYSDEADCHAR: u32 = 0x0107;

/// Handles `USER32.dll!GetAsyncKeyState`.




/// Handles `USER32.dll!PeekMessageA`.


/// Handles `USER32.dll!LoadIconA`.


/// Handles `USER32.dll!LoadCursorA`.


/// Handles `USER32.dll!RegisterClassExW`.


/// Handles `USER32.dll!RegisterClassExA`.


/// Handles `USER32.dll!MessageBoxW`.


/// Handles `USER32.dll!MessageBoxA`.


/// Handles dynamic `USER32.dll!SetProcessDPIAware`.


/// Handles dynamic `USER32.dll!TrackMouseEvent`.


/// Handles `USER32.dll!GetCursorPos`.


/// Handles `USER32.dll!ClipCursor` (accept clip rect or release when NULL).


/// Handles `USER32.dll!GetClipCursor`.


/// Handles `USER32.dll!CallMsgFilterA/W`.
///
/// Returns FALSE so the message continues through the normal dispatch path
/// (no installed WH_MSGFILTER/WH_SYSMSGFILTER hooks).




/// Handles `USER32.dll!GetSystemMetrics`.


/// Handles dynamic `USER32.dll!MonitorFromWindow`.


/// Handles dynamic `USER32.dll!GetMonitorInfoA`.


/// Handles dynamic `USER32.dll!GetMonitorInfoW`.


/// Handles dynamic `USER32.dll!MonitorFromRect`.


/// Handles dynamic `USER32.dll!MonitorFromPoint`.


/// Handles dynamic `USER32.dll!EnumDisplayMonitors`.


/// Handles dynamic `USER32.dll!EnumDisplayDevicesA`.


/// Handles dynamic `USER32.dll!EnumDisplayDevicesW`.


/// Handles `USER32.dll!GetWindowRect`.


/// Handles dynamic `USER32.dll!GetDpiForWindow`.


/// Handles `USER32.dll!PostMessageA`.




/// Handles dynamic `USER32.dll!GetSystemMetricsForDpi`.


/// Handles dynamic `USER32.dll!AdjustWindowRectExForDpi`.


/// Handles `USER32.dll!SetWindowPos`.


/// Handles `USER32.dll!GetDC`.


/// Handles `USER32.dll!SendMessageA`.


/// Handles `USER32.dll!SendMessageW`.




/// Handles `USER32.dll!ReleaseDC`.


/// Handles `USER32.dll!LoadImageA`.


/// Handles `USER32.dll!LoadImageW`.




/// Handles `USER32.dll!SetWindowLongPtrW`.


/// Handles `USER32.dll!DestroyIcon`.


/// Handles `USER32.dll!IsWindow`.


/// Handles `USER32.dll!IsWindowVisible`.


/// Handles `USER32.dll!IsWindowEnabled`.


/// Handles `USER32.dll!GetParent`.


/// Handles `USER32.dll!GetActiveWindow`.


/// Handles `USER32.dll!GetForegroundWindow`.


/// Handles `USER32.dll!ShowWindow`.


/// Handles `USER32.dll!EnableWindow`.


/// Handles `USER32.dll!SetForegroundWindow`.


/// Handles `USER32.dll!SetActiveWindow`.


/// Handles `USER32.dll!SetFocus`.


/// Handles `USER32.dll!GetFocus`.


/// Handles `USER32.dll!SetCapture`.


/// Handles `USER32.dll!GetCapture`.


/// Handles `USER32.dll!ReleaseCapture`.


/// Handles `USER32.dll!SetCursor`.




pub mod dc;
pub mod display;
pub mod input;
pub mod menu;
pub mod message;
pub mod misc;
pub mod window;
pub use dc::*;
pub use display::*;
pub use input::*;
pub use menu::*;
pub use message::*;
pub use misc::*;
pub use window::*;

pub(crate) fn low_i32(value: u64, name: &str) -> Result<i32> {
    let low_value = value & u64::from(u32::MAX);

    let low = u32::try_from(low_value).with_context(|| format!("{name} does not fit u32"))?;

    Ok(i32::from_ne_bytes(low.to_ne_bytes()))
}

pub(crate) fn write_window_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    rect_ptr: u64,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) -> Result<()> {
    write_guest_i32(engine, rect_ptr, left)?;

    write_guest_i32(engine, checked_field_address(rect_ptr, 4, "RECT.top")?, top)?;

    write_guest_i32(
        engine,
        checked_field_address(rect_ptr, 8, "RECT.right")?,
        right,
    )?;

    write_guest_i32(
        engine,
        checked_field_address(rect_ptr, 12, "RECT.bottom")?,
        bottom,
    )?;

    Ok(())
}

pub(crate) fn write_ansi_window_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    buffer_ptr: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    let capacity =
        usize::try_from(max_characters).context("ANSI window text capacity does not fit usize")?;

    let copied = write_guest_ansi_c_string(engine, buffer_ptr, capacity, text)
        .context("failed to write ANSI window text")?;

    u64::try_from(copied).context("ANSI window text length does not fit u64")
}

pub(crate) fn write_wide_window_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    buffer_ptr: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    let capacity =
        usize::try_from(max_characters).context("wide window text capacity does not fit usize")?;

    let copied = write_guest_utf16_c_string(engine, buffer_ptr, capacity, text)
        .context("failed to write wide window text")?;

    u64::try_from(copied).context("wide window text length does not fit u64")
}

/// Handles `USER32.dll!UpdateWindow`.


/// Handles `USER32.dll!InvalidateRect`.


/// Handles `USER32.dll!BeginPaint`.
///
/// Fills `PAINTSTRUCT` with a fake HDC and client rect; real painting is stubbed.


/// Handles `USER32.dll!EndPaint`.


/// Handles `USER32.dll!RedrawWindow`.


/// Handles `USER32.dll!SetWindowTextA`.


/// Handles `USER32.dll!SetWindowTextW`.


/// Handles `USER32.dll!GetWindowTextA`.


/// Handles `USER32.dll!GetWindowTextW`.


/// Handles `USER32.dll!GetClientRect`.


/// Handles `USER32.dll!MoveWindow`.


/// Handles `USER32.dll!ScreenToClient`.


/// Handles `USER32.dll!ClientToScreen`.


/// Handles `USER32.dll!GetDesktopWindow`.


/// Handles `USER32.dll!GetSysColor`.


/// Handles `USER32.dll!GetSysColorBrush`.


/// Handles `USER32.dll!GetDialogBaseUnits`.


/// Handles `USER32.dll!SetRect`.


/// Handles `USER32.dll!IsIconic`.


/// Handles `USER32.dll!IsZoomed`.


/// Handles `USER32.dll!GetWindowThreadProcessId`.


/// Handles `USER32.dll!GetDlgCtrlID`.


/// Handles `USER32.dll!GetCursor`.


/// Handles `USER32.dll!IsChild`.


/// Handles `USER32.dll!GetWindow`.


/// Handles `USER32.dll!SetKeyboardState`.


/// Handles `USER32.dll!GetKeyboardState`.


/// Handles `USER32.dll!GetKeyState`.


/// Handles `USER32.dll!MapVirtualKeyA`.


pub(crate) fn window_long_ptr_index(index_raw: u64, api_name: &str) -> Result<i64> {
    let index_low = u32::try_from(index_raw)
        .with_context(|| format!("{api_name} index does not fit in u32"))?;

    Ok(i64::from(i32::from_ne_bytes(index_low.to_ne_bytes())))
}

pub(crate) fn get_window_long_ptr_value(
    window_handle: u64,
    index_raw: u64,
    state: &WinApiState,
    api_name: &str,
) -> Result<u64> {
    let index = window_long_ptr_index(index_raw, api_name)?;

    Ok(state
        .window_state.window_long_ptr_values
        .iter()
        .find(|(stored_window, stored_index, _)| {
            *stored_window == window_handle && *stored_index == index
        })
        .map_or(0, |(_, _, value)| *value))
}

pub(crate) fn set_window_long_ptr_value(
    window_handle: u64,
    index_raw: u64,
    new_value: u64,
    state: &mut WinApiState,
    api_name: &str,
) -> Result<u64> {
    let index = window_long_ptr_index(index_raw, api_name)?;

    let previous_value = state
        .window_state.window_long_ptr_values
        .iter()
        .find(|(stored_window, stored_index, _)| {
            *stored_window == window_handle && *stored_index == index
        })
        .map_or(0, |(_, _, value)| *value);

    if let Some(entry) =
        state
            .window_state.window_long_ptr_values
            .iter_mut()
            .find(|(stored_window, stored_index, _)| {
                *stored_window == window_handle && *stored_index == index
            })
    {
        entry.2 = new_value;
    } else {
        state
            .window_state.window_long_ptr_values
            .push((window_handle, index, new_value));
    }

    Ok(previous_value)
}

/// Handles `USER32.dll!GetWindowLongPtrA`.


/// Handles `USER32.dll!GetWindowLongPtrW`.


/// Handles `USER32.dll!SetWindowLongPtrA`.


/// Handles `USER32.dll!SetTimer`.


/// Handles `USER32.dll!KillTimer`.


/// Handles `USER32.dll!AdjustWindowRectEx`.


/// Handles `USER32.dll!SetWindowsHookExW`.


/// Handles `USER32.dll!UnhookWindowsHookEx`.


/// Handles `USER32.dll!CallNextHookEx`.


/// Handles `USER32.dll!EnableMenuItem`.


/// Handles `USER32.dll!CheckMenuItem`.


pub(crate) fn write_message_structure(
    engine: &mut dyn wie_cpu::CpuEngine,
    message_address: u64,
    message: &QueuedWindowMessage,
) -> Result<()> {
    write_guest_u64(engine, message_address, message.window_handle)
        .context("failed to write MSG.hwnd")?;

    write_guest_u32(
        engine,
        checked_field_address(message_address, 8, "MSG.message")?,
        message.message,
    )
    .context("failed to write MSG.message")?;

    // Bytes 12..16 are alignment padding on Win64.
    write_guest_u32(
        engine,
        checked_field_address(message_address, 12, "MSG alignment padding")?,
        0,
    )
    .context("failed to clear MSG alignment padding")?;

    write_guest_u64(
        engine,
        checked_field_address(message_address, 16, "MSG.wParam")?,
        message.word_parameter,
    )
    .context("failed to write MSG.wParam")?;

    write_guest_u64(
        engine,
        checked_field_address(message_address, 24, "MSG.lParam")?,
        message.long_parameter,
    )
    .context("failed to write MSG.lParam")?;

    write_guest_u32(
        engine,
        checked_field_address(message_address, 32, "MSG.time")?,
        message.time,
    )
    .context("failed to write MSG.time")?;

    write_guest_i32(
        engine,
        checked_field_address(message_address, 36, "MSG.pt.x")?,
        message.point_x,
    )
    .context("failed to write MSG.pt.x")?;

    write_guest_i32(
        engine,
        checked_field_address(message_address, 40, "MSG.pt.y")?,
        message.point_y,
    )
    .context("failed to write MSG.pt.y")?;

    // MSG.lPrivate on modern Win64 layouts.
    write_guest_u32(
        engine,
        checked_field_address(message_address, 44, "MSG.lPrivate")?,
        0,
    )
    .context("failed to clear MSG.lPrivate")?;

    Ok(())
}

/// Handles `USER32.dll!GetMessageA`.


/// Handles `USER32.dll!TranslateMessage`.


/// Neutral default message handler used by several USER32 `Def*Proc` APIs.


/// Handles `USER32.dll!DefWindowProcA`.


/// Handles `USER32.dll!DefWindowProcW`.


/// Handles `USER32.dll!DefFrameProcA`.


/// Handles `USER32.dll!DefFrameProcW`.


/// Handles `USER32.dll!DefMDIChildProcA`.


/// Handles `USER32.dll!DefMDIChildProcW`.


pub(crate) fn allocate_menu_handle(state: &mut WinApiState) -> Result<u64> {
    let handle = state.window_state.next_menu_handle;
    state.window_state.next_menu_handle = state
        .window_state.next_menu_handle
        .checked_add(1)
        .context("menu handle allocator overflow")?;
    Ok(handle)
}



/// Handles `USER32.dll!GetMenu` — returns the HMENU for a window, or 0.


/// Handles `USER32.dll!CreateMenu`.


/// Handles `USER32.dll!CreatePopupMenu`.


/// Handles `USER32.dll!AppendMenuA`.


/// Handles `USER32.dll!AppendMenuW`.


/// Handles `USER32.dll!SetMenu`.


/// Handles `USER32.dll!DestroyMenu`.


/// Handles `USER32.dll!RemoveMenu`.


/// Handles `USER32.dll!DeleteMenu`.


/// Handles `USER32.dll!ModifyMenuA`.


/// Handles `USER32.dll!ModifyMenuW`.


/// Handles `USER32.dll!GetSystemMenu`.


/// Handles `USER32.dll!TrackPopupMenu`.


/// Handles `USER32.dll!GetMenuItemInfoA`.


/// Handles `USER32.dll!GetMenuItemInfoW`.


/// Handles `USER32.dll!SetMenuItemInfoA`.


/// Handles `USER32.dll!SetMenuItemInfoW`.


/// Handles `USER32.dll!CheckMenuRadioItem`.


/// Handles `USER32.dll!SetScrollInfo`.
///
/// Accepts the call and returns `nPos` (or `nMax` if position is absent) so
/// scroll-range setup during level-editor open does not abort the guest.


/// Handles `USER32.dll!ScrollWindowEx` (no-op success stub).


/// Handles `USER32.dll!ScrollDC` (no-op success stub; no real pixel scroll).


/// Handles `USER32.dll!DispatchMessageA`.


pub(crate) fn register_window_class(state: &mut WinApiState, mut record: WindowClassRecord) -> Result<u64> {
    if record.class_name.is_empty() || record.window_proc == 0 {
        return Ok(0);
    }

    if let Some(existing) = state.window_state.window_classes.iter().find(|existing| {
        existing.class_name.eq_ignore_ascii_case(&record.class_name)
            && existing.unicode == record.unicode
    }) {
        return Ok(u64::from(existing.atom));
    }

    let atom = state.window_state.next_window_class_atom;

    if atom == 0 {
        return Ok(0);
    }

    state.window_state.next_window_class_atom = state
        .window_state.next_window_class_atom
        .checked_add(1)
        .context("window class atom overflow")?;

    record.atom = atom;
    state.window_state.window_classes.push(record);

    Ok(u64::from(atom))
}

#[derive(Debug)]
pub(crate) struct CreateWindowRequest {
    class_identifier: WindowClassIdentifier,
    title: String,
    style: u32,
    extended_style: u32,
    parent_handle: u64,
    menu_handle: u64,
    instance_handle: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug)]
pub(crate) enum WindowClassIdentifier {
    Atom(u16),
    Name(String),
}

pub(crate) fn read_window_class_identifier_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    value: u64,
) -> Result<WindowClassIdentifier> {
    if value == 0 {
        return Ok(WindowClassIdentifier::Name(String::new()));
    }

    if let Ok(atom) = u16::try_from(value) {
        return Ok(WindowClassIdentifier::Atom(atom));
    }

    let name = read_guest_ansi_lossy(engine, value, 256)
        .context("failed to read ANSI window class name")?;

    Ok(WindowClassIdentifier::Name(name))
}

pub(crate) fn read_window_class_identifier_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    value: u64,
) -> Result<WindowClassIdentifier> {
    if value == 0 {
        return Ok(WindowClassIdentifier::Name(String::new()));
    }

    if let Ok(atom) = u16::try_from(value) {
        return Ok(WindowClassIdentifier::Atom(atom));
    }

    let name = read_guest_utf16_lossy(engine, value, 256)
        .context("failed to read Unicode window class name")?;

    Ok(WindowClassIdentifier::Name(name))
}

pub(crate) fn window_class_identifier_matches(
    record: &WindowClassRecord,
    identifier: &WindowClassIdentifier,
) -> bool {
    match identifier {
        WindowClassIdentifier::Atom(atom) => record.atom == *atom,

        WindowClassIdentifier::Name(name) => record.class_name.eq_ignore_ascii_case(name),
    }
}

pub(crate) fn find_window_class<'a>(
    state: &'a WinApiState,
    identifier: &WindowClassIdentifier,
    unicode: bool,
) -> Option<&'a WindowClassRecord> {
    state
        .window_state.window_classes
        .iter()
        .find(|record| {
            record.unicode == unicode && window_class_identifier_matches(record, identifier)
        })
        .or_else(|| {
            state
                .window_state.window_classes
                .iter()
                .find(|record| window_class_identifier_matches(record, identifier))
        })
}

pub(crate) fn create_window_record(
    state: &mut WinApiState,
    request: CreateWindowRequest,
    unicode: bool,
) -> Result<(u64, u64, bool)> {
    let registered_class = find_window_class(state, &request.class_identifier, unicode).cloned();

    let handle = state.window_state.next_window_handle;

    if handle == 0 {
        return Ok((0, 0, unicode));
    }

    state.window_state.next_window_handle = state
        .window_state.next_window_handle
        .checked_add(1)
        .context("fake window handle overflow")?;

    let (class_atom, class_name, window_proc, class_unicode) =
        if let Some(window_class) = registered_class {
            (
                window_class.atom,
                window_class.class_name,
                window_class.window_proc,
                window_class.unicode,
            )
        } else {
            let class_name = match request.class_identifier {
                WindowClassIdentifier::Atom(atom) => {
                    format!("#{atom}")
                }

                WindowClassIdentifier::Name(name) => name,
            };

            /*
             * Classes supplied by USER32, COMCTL32 and other system
             * components are not registered by the guest application.
             * They still receive runtime-owned HWND records, but have no
             * guest WndProc callback.
             */
            (0, class_name, 0, unicode)
        };

    state.window_state.windows.push(WindowRecord {
        handle,
        class_atom,
        class_name,
        window_proc,
        unicode: class_unicode,
        title: request.title,
        style: request.style,
        extended_style: request.extended_style,
        parent_handle: request.parent_handle,
        menu_handle: request.menu_handle,
        instance_handle: request.instance_handle,
        x: request.x,
        y: request.y,
        width: request.width,
        height: request.height,
        visible: false,
        enabled: true,
    });

    Ok((handle, window_proc, class_unicode))
}

pub(crate) fn create_mdi_child_from_struct(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    create_struct_ptr: u64,
    unicode: bool,
) -> Result<u64> {
    if create_struct_ptr == 0 {
        return Ok(0);
    }

    // MDICREATESTRUCTA/W on Win64:
    // +0x00 szClass
    // +0x08 szTitle
    // +0x10 hOwner
    // +0x18 x
    // +0x1c y
    // +0x20 cx
    // +0x24 cy
    // +0x28 style
    // +0x30 lParam
    let class_ptr = read_guest_u64(engine, create_struct_ptr)
        .context("failed to read MDICREATESTRUCT.szClass")?;
    let title_ptr = read_guest_u64(
        engine,
        checked_field_address(create_struct_ptr, 8, "MDICREATESTRUCT.szTitle")?,
    )?;
    let owner = read_guest_u64(
        engine,
        checked_field_address(create_struct_ptr, 16, "MDICREATESTRUCT.hOwner")?,
    )?;
    let x = read_guest_i32(
        engine,
        checked_field_address(create_struct_ptr, 24, "MDICREATESTRUCT.x")?,
    )?;
    let y = read_guest_i32(
        engine,
        checked_field_address(create_struct_ptr, 28, "MDICREATESTRUCT.y")?,
    )?;
    let cx = read_guest_i32(
        engine,
        checked_field_address(create_struct_ptr, 32, "MDICREATESTRUCT.cx")?,
    )?;
    let cy = read_guest_i32(
        engine,
        checked_field_address(create_struct_ptr, 36, "MDICREATESTRUCT.cy")?,
    )?;
    let style = read_guest_u32(
        engine,
        checked_field_address(create_struct_ptr, 40, "MDICREATESTRUCT.style")?,
    )?;

    let class_identifier = if unicode {
        read_window_class_identifier_w(engine, class_ptr)?
    } else {
        read_window_class_identifier_a(engine, class_ptr)?
    };

    let title = if title_ptr == 0 {
        String::new()
    } else if unicode {
        read_guest_utf16_lossy(engine, title_ptr, 512)?
    } else {
        read_guest_ansi_lossy(engine, title_ptr, 512)?
    };

    let (handle, _window_proc, _class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: owner,
            x,
            y,
            width: cx,
            height: cy,
        },
        unicode,
    )?;

    Ok(handle)
}



pub(crate) fn is_known_window(state: &WinApiState, handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    handle == FAKE_WINDOW_HANDLE
        || handle == FAKE_DESKTOP_WINDOW_HANDLE
        || find_window(state, handle).is_some()
        || state.window_state.active_window_handle == handle
        || state.window_state.foreground_window_handle == handle
}


