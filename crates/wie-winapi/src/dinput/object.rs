//! The `IDirectInput8` COM object: vtable construction and the interface's
//! own methods (`CreateDevice`, `EnumDevices`, `GetDeviceStatus`,
//! `SetCooperativeLevel`, `FindDevice`, `EnumDevicesBySemantics`,
//! `RunControlPanel`, `Initialize`, `ConfigureDevices`).
//!
//! The vtable is built exactly the way [`crate::d3d9`] builds
//! `IDirect3D9`'s: one coherent guest allocation holding the vtable followed
//! by the COM object, every one of the 11 slots (dinput.h:2402-2417) pointing
//! at the `FakeVa::Com` stop for that (interface, slot) pair, and the object's
//! first word pointing back at the vtable.

use anyhow::{Context, Result};

use crate::fake_va::{
    DInput8Iface, DirectInput8Method, DirectInputDevice8Method, encode_com_dinput8,
};
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::{read_bytes, write_u64 as write_guest_u64};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::device::dispatch_device_method;
use super::enumerate::dispatch_enumerate_device;
use super::{
    DI_NOTATTACHED, DI_OK, DIERR_INVALIDPARAM, DIERR_NOINTERFACE, DIERR_NOTFOUND,
    DIERR_OLDDIRECTINPUTVERSION, DIERR_UNSUPPORTED, DINPUT_OBJECT_OFFSET,
    DINPUT_VTABLE_ALLOCATION_SIZE, DIRECTINPUT_VERSION_8, DInputDeviceClass, DInputDeviceRecord,
};

/// `IID_IUnknown` = `{00000000-0000-0000-C000-000000000046}` in Win64
/// little-endian `GUID` memory layout. The only IID these objects answer.
const IID_IUNKNOWN: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

/// Fake target VA for `IDirectInput8` vtable slot `slot`.
///
/// The slot byte is the real vtable position (the `IUnknown` trio plus the
/// `IDirectInput` → `2` → `7` → `8` chain, dinput.h:2402-2417), so it encodes
/// straight into the `DInput8Iface::DirectInput8` byte space reserved by
/// `DInput8Iface`.
/// `IDirectInput8` vtable slot `slot`, or an error when `slot` is past the
/// vtable.
pub(crate) fn direct_input8_method_va(slot: usize) -> Result<u64> {
    if slot >= DirectInput8Method::VTABLE_SLOTS {
        anyhow::bail!("IDirectInput8 method slot {slot} out of range");
    }
    let method = u8::try_from(slot).context("IDirectInput8 slot does not fit u8")?;
    Ok(encode_com_dinput8(DInput8Iface::DirectInput8, method))
}

/// Fake target VA for `IDirectInputDevice8` vtable slot `slot`
/// (dinput.h:1992-2031).
/// `IDirectInputDevice8` vtable slot `slot`, or an error when `slot` is past
/// the vtable.
pub(crate) fn direct_input_device8_method_va(slot: usize) -> Result<u64> {
    if slot >= DirectInputDevice8Method::VTABLE_SLOTS {
        anyhow::bail!("IDirectInputDevice8 method slot {slot} out of range");
    }
    let method = u8::try_from(slot).context("IDirectInputDevice8 slot does not fit u8")?;
    Ok(encode_com_dinput8(DInput8Iface::DirectInputDevice8, method))
}

/// Allocate a vtable + object pair from the guest heap.
///
/// Returns `(vtable_address, object_address)`, or `(0, 0)` when the guest heap
/// is exhausted. The allocation covers the largest vtable WIE builds (the
/// 32-slot device vtable, 256 bytes) plus the object pointer.
pub(crate) fn allocate_com_pair(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> (u64, u64) {
    let vtable_address = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, DINPUT_VTABLE_ALLOCATION_SIZE);
    if vtable_address == 0 {
        return (0, 0);
    }
    match vtable_address.checked_add(DINPUT_OBJECT_OFFSET) {
        Some(object_address) => (vtable_address, object_address),
        None => (0, 0),
    }
}

/// Fill a vtable's slots from `method_va` and point the object's first word at
/// the vtable.
fn write_vtable(
    engine: &mut dyn wie_cpu::CpuEngine,
    vtable_address: u64,
    object_address: u64,
    method_va: impl Fn(usize) -> Result<u64>,
    slots: usize,
) -> Result<()> {
    for slot in 0..slots {
        let entry_address = vtable_address
            .checked_add(
                u64::try_from(slot)
                    .context("vtable slot does not fit u64")?
                    .saturating_mul(8),
            )
            .context("vtable entry address overflow")?;
        write_guest_u64(engine, entry_address, method_va(slot)?)?;
    }
    // A COM object starts with a pointer to its vtable.
    write_guest_u64(engine, object_address, vtable_address)
}

/// `HRESULT DirectInput8Create(HINSTANCE, DWORD dwVersion, LPDIRECTINPUT8 *,
/// LPUNKNOWN *)` — the only `dinput8.dll` export (also ordinal 1).
///
/// Win64: `rcx = hinst`, `rdx = dwVersion`, `r8 = lplpdid`, `r9 = ppunk`.
pub(crate) fn direct_input8_create(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let _hinst = read_arg(engine, ArgReg::Rcx, "DirectInput8Create")?;
    let version = read_arg(engine, ArgReg::Rdx, "DirectInput8Create")?;
    let out_device = read_arg(engine, ArgReg::R8, "DirectInput8Create")?;
    let out_unknown = read_arg(engine, ArgReg::R9, "DirectInput8Create")?;

    // A NULL out-pointer is a hard error, not something to paper over.
    if out_device == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    // Real DirectInput8 refuses anything but 0x0800 with
    // DIERR_OLDDIRECTINPUTVERSION rather than quietly negotiating.
    if version != DIRECTINPUT_VERSION_8 {
        return ctx.finish(DIERR_OLDDIRECTINPUTVERSION);
    }

    let (vtable_address, object_address) = allocate_com_pair(engine, state);
    if object_address == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    write_vtable(
        engine,
        vtable_address,
        object_address,
        direct_input8_method_va,
        DirectInput8Method::VTABLE_SLOTS,
    )
    .context("failed to build the IDirectInput8 vtable")?;

    let dinput = state.dinput8();
    dinput.direct_input_object = object_address;
    dinput.ref_count = 1;
    dinput.devices.clear();

    write_guest_u64(engine, out_device, object_address)
        .context("DirectInput8Create out-pointer write")?;
    // `ppunk` is optional; when present it aliases the same object.
    if out_unknown != 0 {
        write_guest_u64(engine, out_unknown, object_address)
            .context("DirectInput8Create ppunk write")?;
    }

    ctx.finish(DI_OK)
}

/// Dispatch an `IDirectInput8::Xxx` vtable stop.
///
/// The name arrives already resolved to its `IDirectInput8::Xxx` trace string
/// by `ComMethod::name`, so this is a match on that interface prefix.
pub(crate) fn dispatch_object_method(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    // The two interfaces share a vtable-slot byte space only in the sense that
    // the trace name disambiguates them; check the device prefix first so a
    // device stop never lands in the object table.
    if let Some(method) = name.strip_prefix("IDirectInputDevice8::") {
        return dispatch_device_method(ctx, method);
    }
    let Some(method) = name.strip_prefix("IDirectInput8::") else {
        return Ok(None);
    };
    let result = match method {
        "QueryInterface" => Some(query_interface(ctx)?),
        "AddRef" => Some(add_ref(ctx, false)?),
        "Release" => Some(release(ctx, false)?),
        "CreateDevice" => Some(create_device(ctx)?),
        "EnumDevices" => Some(dispatch_enumerate_device(ctx)?),
        "GetDeviceStatus" => Some(get_device_status(ctx)?),
        // There is no DirectInput control panel in WIE, and no `Initialize`
        // work to do (one process-wide object, no per-HINSTANCE drivers).
        "RunControlPanel" | "Initialize" => Some(ctx.finish(DI_OK)?),
        "FindDevice" => Some(find_device(ctx)?),
        // No action maps: nothing is bound, so nothing can be enumerated by
        // semantics. `DI_NOTATTACHED` (S_FALSE) is the honest "no devices".
        "EnumDevicesBySemantics" => Some(ctx.finish(DI_NOTATTACHED)?),
        // Device *configuration* (the install wizard) does not exist in WIE.
        "ConfigureDevices" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        _ => None,
    };
    Ok(result)
}

/// Read a 16-byte `GUID` at `va`, or `None` when the address is NULL or the
/// read fails.
fn read_guid(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Option<[u8; 16]> {
    if va == 0 {
        return None;
    }
    let mut guid = [0_u8; 16];
    read_bytes(engine, va, &mut guid).ok()?;
    Some(guid)
}

/// `IUnknown::QueryInterface` — only `IID_IUnknown` is answerable. Anything
/// else (a real `IID_IDirectInput8W`, a `IID_IDirectInputDevice8W` the guest
/// copied from the SDK) gets a NULLed out-pointer and `DIERR_NOINTERFACE`,
/// which is the honest answer: WIE hands out these two interfaces and no
/// others.
pub(super) fn query_interface(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "QueryInterface")?;
    let riid = read_arg(engine, ArgReg::Rdx, "QueryInterface")?;
    let out = read_arg(engine, ArgReg::R8, "QueryInterface")?;

    let known = read_guid(engine, riid) == Some(IID_IUNKNOWN);
    if out != 0 {
        write_guest_u64(engine, out, if known { this } else { 0 })?;
    }
    if !known {
        return ctx.finish(DIERR_NOINTERFACE);
    }
    ctx.finish(DI_OK)
}

pub(super) fn add_ref(ctx: &mut HandlerContext<'_>, device: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "AddRef")?;
    let state = &mut *ctx.state;
    let count = if device {
        match state.dinput8().device_mut(this) {
            Some(record) => {
                record.ref_count = record.ref_count.saturating_add(1);
                record.ref_count
            }
            None => 0,
        }
    } else {
        let dinput = state.dinput8();
        dinput.ref_count = dinput.ref_count.saturating_add(1);
        dinput.ref_count
    };
    ctx.finish(count)
}

pub(super) fn release(ctx: &mut HandlerContext<'_>, device: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "Release")?;
    let state = &mut *ctx.state;
    let count = if device {
        let dinput = state.dinput8();
        let Some(record) = dinput.device_mut(this) else {
            return ctx.finish(0);
        };
        record.ref_count = record.ref_count.saturating_sub(1);
        let remaining = record.ref_count;
        if remaining == 0 {
            dinput
                .devices
                .retain(|candidate| candidate.object_address != this);
        }
        remaining
    } else {
        let dinput = state.dinput8();
        dinput.ref_count = dinput.ref_count.saturating_sub(1);
        if dinput.ref_count == 0 {
            dinput.direct_input_object = 0;
            dinput.devices.clear();
        }
        dinput.ref_count
    };
    ctx.finish(count)
}

/// `HRESULT CreateDevice(REFGUID rguid, LPDIRECTINPUTDEVICE8 **, LPUNKNOWN)`.
///
/// Only the two device GUIDs `EnumDevices` hands out exist. A guest asking for
/// a joystick, a gamepad, a flight control, or a real machine's HID GUID gets
/// `DIERR_INVALIDPARAM` — never a device object that silently does nothing.
fn create_device(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let _this = read_arg(engine, ArgReg::Rcx, "CreateDevice")?;
    let guid_va = read_arg(engine, ArgReg::Rdx, "CreateDevice")?;
    let out = read_arg(engine, ArgReg::R8, "CreateDevice")?;
    let _outer = read_arg(engine, ArgReg::R9, "CreateDevice")?;

    if out == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    // Fail the call rather than leave the guest's out-pointer untouched.
    write_guest_u64(engine, out, 0).context("CreateDevice out-pointer clear")?;

    let Some(guid) = read_guid(engine, guid_va) else {
        return ctx.finish(DIERR_INVALIDPARAM);
    };
    let Some(class) = DInputDeviceClass::from_instance_guid(&guid)
        .or_else(|| DInputDeviceClass::from_product_guid(&guid))
    else {
        return ctx.finish(DIERR_INVALIDPARAM);
    };

    let (vtable_address, object_address) = allocate_com_pair(engine, state);
    if object_address == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    write_vtable(
        engine,
        vtable_address,
        object_address,
        direct_input_device8_method_va,
        DirectInputDevice8Method::VTABLE_SLOTS,
    )
    .context("failed to build the IDirectInputDevice8 vtable")?;

    state
        .dinput8()
        .devices
        .push(DInputDeviceRecord::new(object_address, class));

    write_guest_u64(engine, out, object_address).context("CreateDevice out-pointer write")?;
    ctx.finish(DI_OK)
}

/// `HRESULT GetDeviceStatus(REFGUID rguidInstance)` — `DI_OK` for the two
/// devices WIE has, `DI_NOTATTACHED` (`S_FALSE`) for everything else, which is
/// the honest "not plugged in" answer a DirectInput guest already knows how to
/// handle.
fn get_device_status(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "GetDeviceStatus")?;
    let guid_va = read_arg(engine, ArgReg::Rdx, "GetDeviceStatus")?;

    let attached = read_guid(engine, guid_va)
        .is_some_and(|guid| DInputDeviceClass::from_instance_guid(&guid).is_some());
    ctx.finish(if attached { DI_OK } else { DI_NOTATTACHED })
}

/// `HRESULT FindDevice(REFGUID rguid, LPCWSTR pszName, LPGUID pguidInstance)`.
///
/// Matches `rguid` against the synthetic product GUIDs and, when it matches,
/// writes the instance GUID. A non-NULL `pszName` must also match the product
/// name (case-insensitively); no match is `DIERR_NOTFOUND`.
fn find_device(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "FindDevice")?;
    let guid_va = read_arg(engine, ArgReg::Rdx, "FindDevice")?;
    let name_va = read_arg(engine, ArgReg::R8, "FindDevice")?;
    let out = read_arg(engine, ArgReg::R9, "FindDevice")?;

    if out == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    let Some(class) =
        read_guid(engine, guid_va).and_then(|guid| DInputDeviceClass::from_product_guid(&guid))
    else {
        return ctx.finish(DIERR_NOTFOUND);
    };
    if name_va != 0 {
        let wanted = crate::guest_string::read_utf16_lossy(engine, name_va, 260)
            .unwrap_or_default()
            .trim_end_matches('\0')
            .to_string();
        if !wanted.eq_ignore_ascii_case(class.product_name()) {
            return ctx.finish(DIERR_NOTFOUND);
        }
    }
    engine
        .mem_write(out, &class.instance_guid())
        .context("FindDevice pguidInstance write")?;
    ctx.finish(DI_OK)
}
