//! USER32 boot-surface stubs (Phase 2): the display/keyboard/clipboard/raw
//! input names DOOM Retro / SDL2 probe at startup. Honest minimal state where
//! cheap; no-op success where the operation has no host counterpart.

use super::{Context, HandlerContext, Result, WinApiHandlerResult, low_i32};
use crate::guest_memory::write_u32;
use zerocopy::IntoBytes;

/// Win32 `ERROR_INVALID_PARAMETER` (raw-input probes return it).
const ERROR_INVALID_PARAMETER: u32 = 87;

/// Handles `USER32.dll!AttachThreadInput` — single input queue, always attached.
pub fn handle_attach_thread_input(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _attach_thread = ctx.engine.read_rcx()?;
    let _attach_to = ctx.engine.read_rdx()?;
    let _attach = ctx.engine.read_r8()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!BringWindowToTop` — reorders the window to the top of
/// the z-order when it exists.
pub fn handle_bring_window_to_top(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for BringWindowToTop")?;
    if crate::user32::window::find_window(state, hwnd).is_some() {
        state
            .present()
            .z_order_to_top(crate::handles::Hwnd::from(hwnd));
        return ctx.finish(1);
    }
    ctx.finish(0)
}
/// Handles `USER32.dll!ChangeDisplaySettingsExW` — DISP_CHANGE_SUCCESSFUL.
pub fn handle_change_display_settings_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _device = ctx.engine.read_rcx()?;
    let _mode = ctx.engine.read_rdx()?;
    let _window = ctx.engine.read_r8()?;
    let _flags = ctx.engine.read_r9()?;
    ctx.finish(0) // DISP_CHANGE_SUCCESSFUL
}
/// Handles `USER32.dll!CopyImage` — no image copying; returns NULL.
pub fn handle_copy_image(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _image = ctx.engine.read_rcx()?;
    let _type = ctx.engine.read_rdx()?;
    let _cx = ctx.engine.read_r8()?;
    let _cy = ctx.engine.read_r9()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!CreateIconFromResource` — returns NULL.
pub fn handle_create_icon_from_resource(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _bits = ctx.engine.read_rcx()?;
    let _bytes = ctx.engine.read_rdx()?;
    let _load = ctx.engine.read_r8()?;
    let _version = ctx.engine.read_r9()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!CreateIconIndirect` — returns NULL.
pub fn handle_create_icon_indirect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _icon_info = ctx.engine.read_rcx()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!DialogBoxIndirectParamW` — no indirect dialog templates
/// are supported; returns `-1` (Windows' "could not create the dialog box").
pub fn handle_dialog_box_indirect_param_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _instance = ctx.engine.read_rcx()?;
    let _template = ctx.engine.read_rdx()?;
    let _parent = ctx.engine.read_r8()?;
    let _dialog_proc = ctx.engine.read_r9()?;
    ctx.finish(u64::from(u32::MAX)) // -1
}
/// Handles `USER32.dll!EnumDisplaySettingsW`.
///
/// Mode 0 reports a 1024x768@60 DEVMODE (a plausible default); any later mode
/// returns FALSE (the enumeration is exhausted).
pub fn handle_enum_display_settings_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device = engine.read_rcx()?;
    let mode_index = engine
        .read_rdx()
        .context("failed to read RDX for EnumDisplaySettingsW")?;
    let mode_va = engine
        .read_r8()
        .context("failed to read R8 for EnumDisplaySettingsW")?;
    if mode_index != 0 || mode_va == 0 {
        return ctx.finish(0);
    }
    // DEVMODEW layout (offsets that SDL reads):
    //   +0x00 wSize (u16), +0x16 dmBitsPerPel (u32), +0x1C dmPelsWidth (u32),
    //   +0x20 dmPelsHeight (u32), +0x28 dmDisplayFrequency (u32)
    let mut buf = [0_u8; 0x40];
    buf[0..2].copy_from_slice(&(0x40_u16).to_le_bytes()); // wSize
    buf[0x16..0x1A].copy_from_slice(&32_u32.to_le_bytes()); // 32bpp
    buf[0x1C..0x20].copy_from_slice(&1024_u32.to_le_bytes());
    buf[0x20..0x24].copy_from_slice(&768_u32.to_le_bytes());
    buf[0x28..0x2C].copy_from_slice(&60_u32.to_le_bytes()); // refresh
    engine
        .mem_write(mode_va, &buf)
        .context("failed to write DEVMODEW")?;
    ctx.finish(1)
}
/// Handles `USER32.dll!FlashWindowEx` — no-op success.
pub fn handle_flash_window_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _flash_info = ctx.engine.read_rcx()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!GetClassInfoExW` — resolves the registered class and
/// fills the `WNDCLASSEXW` from its record.
pub fn handle_get_class_info_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _instance = engine.read_rcx()?;
    let class_name_va = engine
        .read_rdx()
        .context("failed to read RDX for GetClassInfoExW")?;
    let out_va = engine
        .read_r8()
        .context("failed to read R8 for GetClassInfoExW")?;
    let class_name = super::read_guest_utf16_lossy(engine, class_name_va, 256)?;
    let identifier = super::WindowClassIdentifier::Name(class_name);
    let Some(record) = super::find_window_class(state, &identifier, true).cloned() else {
        return ctx.finish(0);
    };
    if out_va != 0 {
        let class = crate::guest_layout::WndClassEx {
            cb_size: 48,
            style: record.style,
            window_proc: record.window_proc,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            instance_handle: record.instance_handle,
            icon_handle: record.icon_handle,
            cursor_handle: record.cursor_handle,
            background_brush: record.background_brush,
            menu_name: record.menu_name,
            class_name_ptr: 0,
            small_icon_handle: record.small_icon_handle,
        };
        engine
            .mem_write(out_va, class.as_bytes())
            .context("failed to write WNDCLASSEXW")?;
    }
    ctx.finish(1)
}
/// Handles `USER32.dll!GetClipboardSequenceNumber` — no clipboard changes.
pub fn handle_get_clipboard_sequence_number(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}
/// Handles `USER32.dll!GetDoubleClickTime` — the Windows default (500 ms).
pub fn handle_get_double_click_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(500)
}
/// Handles `USER32.dll!GetKeyboardLayout` — a plausible default HKL (0).
pub fn handle_get_keyboard_layout(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _tid = ctx.engine.read_rcx()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!GetMessageExtraInfo` — no extra info.
pub fn handle_get_message_extra_info(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}
/// Handles `USER32.dll!GetMessageTime` — no message timestamps kept.
pub fn handle_get_message_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}
/// Handles `USER32.dll!GetPropW`.
pub fn handle_get_prop_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for GetPropW")?;
    let name_va = engine
        .read_rdx()
        .context("failed to read RDX for GetPropW")?;
    let name = super::read_guest_utf16_lossy(engine, name_va, 256)?;
    let value = state
        .window_state()
        .window_props
        .iter()
        .find(|(h, n, _)| *h == hwnd && *n == name)
        .map(|(_, _, v)| *v)
        .unwrap_or(0);
    ctx.finish(value)
}
/// Handles `USER32.dll!GetRawInputData` — no raw-input devices; fails with
/// `ERROR_INVALID_PARAMETER`.
pub fn handle_get_raw_input_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _input = ctx.engine.read_rcx()?;
    let _command = ctx.engine.read_rdx()?;
    let _data = ctx.engine.read_r8()?;
    let _size_va = ctx.engine.read_r9()?;
    ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
    ctx.finish(u64::from(u32::MAX)) // (UINT)-1
}
/// Handles `USER32.dll!GetRawInputDeviceInfoA` — no devices.
pub fn handle_get_raw_input_device_info_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _device = ctx.engine.read_rcx()?;
    let _command = ctx.engine.read_rdx()?;
    let _data = ctx.engine.read_r8()?;
    let _size_va = ctx.engine.read_r9()?;
    ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
    ctx.finish(u64::from(u32::MAX))
}
/// Handles `USER32.dll!GetRawInputDeviceList` — zero devices; the count slot
/// is written with 0.
pub fn handle_get_raw_input_device_list(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _list = engine.read_rcx()?;
    let count_va = engine
        .read_rdx()
        .context("failed to read RDX for GetRawInputDeviceList")?;
    let _size = engine.read_r8()?;
    if count_va != 0 {
        write_u32(engine, count_va, 0)?;
    }
    ctx.finish(0)
}
/// Handles `USER32.dll!GetUpdateRect` — no dirty region tracked; FALSE.
pub fn handle_get_update_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _rect = ctx.engine.read_rdx()?;
    let _erase = ctx.engine.read_r8()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!GetWindowLongW` — same value as `GetWindowLongPtrW`.
pub fn handle_get_window_long_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    crate::user32::window::handle_get_window_long_ptr_w(ctx)
}
/// Handles `USER32.dll!IntersectRect` — real rectangle intersection.
pub fn handle_intersect_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let out_va = engine
        .read_rcx()
        .context("failed to read RCX for IntersectRect")?;
    let r1_va = engine
        .read_rdx()
        .context("failed to read RDX for IntersectRect")?;
    let r2_va = engine
        .read_r8()
        .context("failed to read R8 for IntersectRect")?;
    if out_va == 0 || r1_va == 0 || r2_va == 0 {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let mut rect = |va: u64| -> Result<[i32; 4]> {
        let mut b = [0_u8; 16];
        engine.mem_read(va, &mut b).context("failed to read RECT")?;
        let mut out = [0_i32; 4];
        for (i, chunk) in b.chunks(4).enumerate() {
            out[i] = i32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
        }
        Ok(out)
    };
    let r1 = rect(r1_va)?;
    let r2 = rect(r2_va)?;
    let left = r1[0].max(r2[0]);
    let top = r1[1].max(r2[1]);
    let right = r1[2].min(r2[2]);
    let bottom = r1[3].min(r2[3]);
    if left >= right || top >= bottom {
        return ctx.finish(0); // empty intersection
    }
    let mut out = [0_u8; 16];
    for (i, v) in [left, top, right, bottom].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    engine
        .mem_write(out_va, &out)
        .context("failed to write IntersectRect result")?;
    ctx.finish(1)
}
/// Handles `USER32.dll!keybd_event` — no-op.
pub fn handle_keybd_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _vk = ctx.engine.read_rcx()?;
    let _scan = ctx.engine.read_rdx()?;
    let _flags = ctx.engine.read_r8()?;
    let _extra = ctx.engine.read_r9()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!MapVirtualKeyW` — same mapping as `MapVirtualKeyA`.
pub fn handle_map_virtual_key_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    crate::user32::input::handle_map_virtual_key_a(ctx)
}
/// Handles `USER32.dll!MsgWaitForMultipleObjects` — same wait semantics as
/// `WaitForMultipleObjects` (the message-wait flag is accepted and ignored).
pub fn handle_msg_wait_for_multiple_objects(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    crate::kernel32::handle_wait_for_multiple_objects(ctx)
}
/// Handles `USER32.dll!PostThreadMessageW` — TRUE for any thread id (messages
/// to thread queues are not delivered, but the call succeeds).
pub fn handle_post_thread_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _tid = ctx.engine.read_rcx()?;
    let _msg = ctx.engine.read_rdx()?;
    let _wparam = ctx.engine.read_r8()?;
    let _lparam = ctx.engine.read_r9()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!PtInRect` — real point-in-rect test (right/bottom
/// edges are exclusive, per Win32).
pub fn handle_pt_in_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_va = engine
        .read_rcx()
        .context("failed to read RCX for PtInRect")?;
    let x = low_i32(engine.read_rdx()?, "PtInRect x")?;
    let y = low_i32(engine.read_r8()?, "PtInRect y")?;
    let mut b = [0_u8; 16];
    engine
        .mem_read(rect_va, &mut b)
        .context("failed to read RECT")?;
    let mut r = [0_i32; 4];
    for (i, chunk) in b.chunks(4).enumerate() {
        r[i] = i32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
    }
    let inside = x >= r[0] && x < r[2] && y >= r[1] && y < r[3];
    ctx.finish(u64::from(inside))
}
/// Handles `USER32.dll!RegisterDeviceNotificationW` — no device notifications.
pub fn handle_register_device_notification_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _recipient = ctx.engine.read_rcx()?;
    let _filter = ctx.engine.read_rdx()?;
    let _flags = ctx.engine.read_r8()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!RegisterHotKey` — accepted, no host hotkeys installed.
pub fn handle_register_hot_key(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _id = ctx.engine.read_rdx()?;
    let _modifiers = ctx.engine.read_r8()?;
    let _vk = ctx.engine.read_r9()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!RegisterRawInputDevices` — FALSE: WIE has no raw-input
/// devices, so SDL2 falls back to the WM_MOUSE* bridge.
pub fn handle_register_raw_input_devices(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _devices = ctx.engine.read_rcx()?;
    let _count = ctx.engine.read_rdx()?;
    let _size = ctx.engine.read_r8()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!RemovePropW`.
pub fn handle_remove_prop_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for RemovePropW")?;
    let name_va = engine
        .read_rdx()
        .context("failed to read RDX for RemovePropW")?;
    let name = super::read_guest_utf16_lossy(engine, name_va, 256)?;
    let props = &mut state.window_state().window_props;
    let idx = props.iter().position(|(h, n, _)| *h == hwnd && *n == name);
    match idx {
        Some(i) => {
            let (_, _, value) = props.remove(i);
            ctx.finish(value)
        }
        None => ctx.finish(0),
    }
}
/// Handles `USER32.dll!SetCursorPos` — accepted; the host cursor is not moved.
pub fn handle_set_cursor_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _x = ctx.engine.read_rcx()?;
    let _y = ctx.engine.read_rdx()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!SetLayeredWindowAttributes` — no-op success.
pub fn handle_set_layered_window_attributes(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _color_key = ctx.engine.read_rdx()?;
    let _alpha = ctx.engine.read_r8()?;
    let _flags = ctx.engine.read_r9()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!SetPropW`.
pub fn handle_set_prop_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for SetPropW")?;
    let name_va = engine
        .read_rdx()
        .context("failed to read RDX for SetPropW")?;
    let value = engine.read_r8().context("failed to read R8 for SetPropW")?;
    let name = super::read_guest_utf16_lossy(engine, name_va, 256)?;
    let props = &mut state.window_state().window_props;
    match props.iter_mut().find(|(h, n, _)| *h == hwnd && *n == name) {
        Some(slot) => slot.2 = value,
        None => props.push((hwnd, name, value)),
    }
    ctx.finish(1)
}
/// Handles `USER32.dll!SetWindowRgn` — the region is not tracked; success.
pub fn handle_set_window_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _region = ctx.engine.read_rdx()?;
    let _redraw = ctx.engine.read_r8()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!SystemParametersInfoA` / `SystemParametersInfoW`.
///
/// The common GET queries SDL probes are answered honestly (work area =
/// 1920x1080, screen-saver inactive, …); unknown actions return FALSE.
pub fn handle_system_parameters_info_impl(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let action = engine
        .read_rcx()
        .context("failed to read RCX for SystemParametersInfo")?;
    let param = engine
        .read_rdx()
        .context("failed to read RDX for SystemParametersInfo")?;
    let value_va = engine
        .read_r8()
        .context("failed to read R8 for SystemParametersInfo")?;
    let _modify = engine.read_r9()?;
    const SPI_GETWORKAREA: u64 = 0x0030;
    const SPI_GETSCREENSAVEACTIVE: u64 = 0x0010;
    const SPI_GETPOWEROFFACTIVE: u64 = 0x0014;
    const SPI_GETSCREENSAVERTIMEOUT: u64 = 0x000E;
    const SPI_GETMOUSE: u64 = 0x0003;
    match action {
        SPI_GETWORKAREA if value_va != 0 => {
            // Full-screen work area (1920x1080).
            let mut b = [0_u8; 16];
            b[0..4].copy_from_slice(&0_i32.to_le_bytes());
            b[4..8].copy_from_slice(&0_i32.to_le_bytes());
            b[8..12].copy_from_slice(&1920_i32.to_le_bytes());
            b[12..16].copy_from_slice(&1080_i32.to_le_bytes());
            engine
                .mem_write(value_va, &b)
                .context("failed to write SPI work area")?;
            ctx.finish(1)
        }
        SPI_GETSCREENSAVEACTIVE | SPI_GETPOWEROFFACTIVE | SPI_GETSCREENSAVERTIMEOUT
            if value_va != 0 =>
        {
            write_u32(engine, value_va, 0)?;
            ctx.finish(1)
        }
        SPI_GETMOUSE if value_va != 0 => {
            // Three 32-bit mouse params (thresholds + acceleration).
            for i in 0_u64..3 {
                write_u32(engine, value_va.wrapping_add(i.wrapping_mul(4)), 0)?;
            }
            ctx.finish(1)
        }
        _ => {
            let _unused = param;
            ctx.finish(0)
        }
    }
}
/// Handles `USER32.dll!SystemParametersInfoA`.
pub fn handle_system_parameters_info_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_system_parameters_info_impl(ctx)
}
/// Handles `USER32.dll!SystemParametersInfoW`.
pub fn handle_system_parameters_info_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_system_parameters_info_impl(ctx)
}
/// Handles `USER32.dll!ToUnicode` — no keyboard translation; returns 0.
pub fn handle_to_unicode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _vk = ctx.engine.read_rcx()?;
    let _scan = ctx.engine.read_rdx()?;
    let _keys = ctx.engine.read_r8()?;
    let _buffer = ctx.engine.read_r9()?;
    ctx.finish(0)
}
/// Handles `USER32.dll!UnregisterDeviceNotification` — no notifications.
pub fn handle_unregister_device_notification(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _handle = ctx.engine.read_rcx()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!UnregisterHotKey` — no hotkeys installed.
pub fn handle_unregister_hot_key(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _id = ctx.engine.read_rdx()?;
    ctx.finish(1)
}
/// Handles `USER32.dll!WaitForInputIdle` — the process is immediately idle
/// (WAIT_OBJECT_0).
pub fn handle_wait_for_input_idle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _process = ctx.engine.read_rcx()?;
    let _timeout = ctx.engine.read_rdx()?;
    ctx.finish(0)
}
