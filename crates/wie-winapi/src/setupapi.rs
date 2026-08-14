//! Handles `SETUPAPI.dll` + `CFGMGR32.dll` — device setup (string dispatch).
//!
//! No host devices: enumeration APIs return an empty device list.
//! Stateless (handles are opaque), so no `DllStateMap` slot is needed.

use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Opaque `HDEVINFO` fake handle returned by `SetupDiGetClassDevs*`.
///
/// Nothing dereferences it; callers only pass it back to us. The value is
/// deliberately outside any real guest allocation range.
const FAKE_DEVINFO_SET: u64 = 0x5200_0001;
/// Win32 `ERROR_NO_MORE_ITEMS` (259) — the device list is empty.
const ERROR_NO_MORE_ITEMS: u32 = 259;

/// Dispatch a `SETUPAPI.dll` / `CFGMGR32.dll` export by name.
///
/// CFGMGR32 exports arrive as `CM_Get_Device_ID_List*`, which lowercase to
/// the `cm_*` arms below.
pub fn dispatch_setupapi(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "setupdigetclassdevsw" | "setupdigetclassdevsa" => {
            Ok(Some(handle_setup_di_get_class_devs(ctx)?))
        }
        "setupdienumdeviceinfo" => Ok(Some(handle_setup_di_enum_device_info(ctx)?)),
        "setupdidestroydeviceinfolist" => Ok(Some(handle_setup_di_destroy_device_info_list(ctx)?)),
        "cm_get_device_id_listw" => Ok(Some(handle_cm_get_device_id_list(ctx, true)?)),
        "cm_get_device_id_lista" => Ok(Some(handle_cm_get_device_id_list(ctx, false)?)),
        // Phase-3 stub wave: devnode-level queries report no such device.
        "cm_get_device_ida" => Ok(Some(handle_cm_no_such_devnode(ctx)?)),
        "cm_get_parent" => Ok(Some(handle_cm_no_such_devnode(ctx)?)),
        "cm_locate_devnodea" => Ok(Some(handle_cm_no_such_devnode(ctx)?)),
        "setupdienumdeviceinterfaces" => Ok(Some(handle_setup_di_no_more_items(ctx)?)),
        "setupdigetdeviceinterfacedetaila" => Ok(Some(handle_setup_di_no_more_items(ctx)?)),
        "setupdigetdeviceregistrypropertya" => Ok(Some(handle_setup_di_no_more_items(ctx)?)),
        _ => Ok(None),
    }
}

/// `HDEVINFO SetupDiGetClassDevsW/A(ClassGuid*, Enumerator, HWND, DWORD Flags)`.
///
/// Returns a fake non-null handle so callers proceed with an (empty) device
/// set instead of bailing on `INVALID_HANDLE_VALUE`.
fn handle_setup_di_get_class_devs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _class_guid = engine
        .read_rcx()
        .context("failed to read RCX for SetupDiGetClassDevs")?;
    let _enumerator = engine
        .read_rdx()
        .context("failed to read RDX for SetupDiGetClassDevs")?;
    let _hwnd_parent = engine
        .read_r8()
        .context("failed to read R8 for SetupDiGetClassDevs")?;
    let _flags = engine
        .read_r9()
        .context("failed to read R9 for SetupDiGetClassDevs")?;
    ctx.finish(FAKE_DEVINFO_SET)
}

/// `BOOL SetupDiEnumDeviceInfo(HDEVINFO, DWORD MemberIndex, PSP_DEVINFO_DATA)`.
///
/// No host devices — always FALSE with `ERROR_NO_MORE_ITEMS`.
fn handle_setup_di_enum_device_info(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _dev_info_set = engine
        .read_rcx()
        .context("failed to read RCX for SetupDiEnumDeviceInfo")?;
    let _member_index = engine
        .read_rdx()
        .context("failed to read RDX for SetupDiEnumDeviceInfo")?;
    let _dev_info_data = engine
        .read_r8()
        .context("failed to read R8 for SetupDiEnumDeviceInfo")?;
    ctx.state.process.last_error = ERROR_NO_MORE_ITEMS;
    ctx.finish(0)
}

/// `BOOL SetupDiDestroyDeviceInfoList(HDEVINFO)` — TRUE.
fn handle_setup_di_destroy_device_info_list(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _dev_info_set = engine
        .read_rcx()
        .context("failed to read RCX for SetupDiDestroyDeviceInfoList")?;
    ctx.finish(1)
}

/// `CONFIGRET CM_Get_Device_ID_ListW/A(Buffer, BufferLen, Flags)`.
///
/// Empty device list: leave a single NUL terminator in the buffer and return
/// `CR_SUCCESS`. A too-small buffer is left untouched (the caller re-queries
/// with `CR_BUFFER_SMALL` semantics elsewhere — the first byte stays 0 from
/// the zero-filled BSS either way).
fn handle_cm_get_device_id_list(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buffer = engine
        .read_rcx()
        .context("failed to read RCX for CM_Get_Device_ID_List")?;
    let buffer_len = engine
        .read_rdx()
        .context("failed to read RDX for CM_Get_Device_ID_List")?;
    let _flags = engine
        .read_r8()
        .context("failed to read R8 for CM_Get_Device_ID_List")?;
    if buffer != 0 {
        if wide {
            if buffer_len >= 2 {
                engine.mem_write(buffer, &[0_u8; 2])?;
            }
        } else if buffer_len >= 1 {
            engine.mem_write(buffer, &[0_u8; 1])?;
        }
    }
    ctx.finish(0) // CR_SUCCESS
}

/// `CR_NO_SUCH_DEVNODE` (cfgmgr32.h) — the devnode does not exist.
const CR_NO_SUCH_DEVNODE: u64 = 0x0E;

/// Shared `CONFIGRET` for the `CM_*` devnode queries — no host devices.
fn handle_cm_no_such_devnode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _arg0 = ctx.engine.read_rcx()?;
    let _arg1 = ctx.engine.read_rdx()?;
    let _arg2 = ctx.engine.read_r8()?;
    let _arg3 = ctx.engine.read_r9()?;
    ctx.finish(CR_NO_SUCH_DEVNODE)
}

/// Shared FALSE for the `SetupDiGet*` interface/property queries — the empty
/// device set yields no interfaces, so `ERROR_NO_MORE_ITEMS`.
fn handle_setup_di_no_more_items(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _arg0 = ctx.engine.read_rcx()?;
    let _arg1 = ctx.engine.read_rdx()?;
    let _arg2 = ctx.engine.read_r8()?;
    let _arg3 = ctx.engine.read_r9()?;
    ctx.state.process.last_error = ERROR_NO_MORE_ITEMS;
    ctx.finish(0)
}
