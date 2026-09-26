//! The `IDirectInputDevice8` COM object: `SetDataFormat`,
//! `SetCooperativeLevel`, `GetCapabilities`, `GetDeviceState`, `GetDeviceInfo`,
//! `Poll`, `Acquire`/`Unacquire`, and the honest-failure surface for
//! everything WIE has no implementation for.
//!
//! The interesting method is `GetDeviceState`: it synthesizes the
//! `DIDEVICEOBJECTDATA` array the *guest's own* `DIDATAFORMAT` describes, out
//! of the live keyboard/cursor state. No report offset is hardcoded — the
//! guest's `DIOBJECTDATAFORMAT` array is the single source of truth for how
//! many objects there are, where each one lives, and therefore which value of
//! the raw report it gets. That is the same contract real DirectInput has.

use anyhow::{Context, Result};
use std::ops::Deref;

use zerocopy::IntoBytes;

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::{
    DiDataFormat, DiDevCaps, DiDeviceInstance, DiDeviceObjectData, DiMouseState2,
    DiObjectDataFormat,
};
use crate::guest_memory::{read_u32, with_typed_read, with_typed_write};

use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::object::{add_ref, query_interface, release};
use super::{
    DI_NOTATTACHED, DI_OK, DIERR_INVALIDPARAM, DIERR_NOTACQUIRED, DIERR_NOTFOUND,
    DIERR_UNSUPPORTED, DInputDeviceClass, DInputDeviceRecord, GuestDataFormat,
};

/// `DIDC_ATTACHED` (dinput.h:924) — the only capability bit WIE sets, and it
/// says the obvious thing: the device is there.
const DIDC_ATTACHED: u32 = 0x0000_0001;
/// `DIDC_EMULATED` (dinput.h:926) — WIE's devices are host-synthesized, not
/// backed by real hardware, and a guest that cares can see that.
const DIDC_EMULATED: u32 = 0x0000_0004;
/// `DIDC_POLLED` (dinput.h:925) — a polled device, which is the only kind WIE
/// has: state is read on demand, there is no hardware event ring.
const DIDC_POLLED: u32 = 0x0000_0002;

/// `DIDEVICEOBJECTDATA` (dinput.h:715-723) is 24 bytes on DI8 (the `uAppData`
/// tail at offset 0x10 is present because `DIRECTINPUT_VERSION` is 0x0800).
const OBJECT_DATA_SIZE: u32 = 24;

/// Dispatch an `IDirectInputDevice8::Xxx` vtable stop. `method` is the part
/// after the `IDirectInputDevice8::` prefix.
pub(crate) fn dispatch_device_method(
    ctx: &mut HandlerContext<'_>,
    method: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match method {
        "QueryInterface" => Some(query_interface(ctx)?),
        "AddRef" => Some(add_ref(ctx, true)?),
        "Release" => Some(release(ctx, true)?),
        "GetCapabilities" => Some(get_capabilities(ctx)?),
        "EnumObjects" => Some(enum_objects(ctx)?),
        "GetProperty" => Some(get_property(ctx)?),
        "SetProperty" => Some(ctx.finish(DI_OK)?),
        "Acquire" => Some(acquire(ctx, true)?),
        "Unacquire" => Some(acquire(ctx, false)?),
        "GetDeviceState" => Some(get_device_state(ctx)?),
        // This is polled state, not a buffered event ring: there is no
        // per-object event stream to hand back.
        "GetDeviceData" | "SendDeviceData" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "SetDataFormat" => Some(set_data_format(ctx)?),
        "SetEventNotification" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "SetCooperativeLevel" => Some(set_cooperative_level(ctx)?),
        "GetObjectInfo" => Some(ctx.finish(DIERR_NOTFOUND)?),
        "GetDeviceInfo" => Some(get_device_info(ctx)?),
        "RunControlPanel" => Some(ctx.finish(DI_OK)?),
        "Initialize" => Some(ctx.finish(DI_OK)?),
        "CreateEffect" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "EnumEffects" => Some(ctx.finish(DI_NOTATTACHED)?),
        "GetEffectInfo" => Some(ctx.finish(DIERR_NOTFOUND)?),
        "GetForceFeedbackState" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "SendForceFeedbackCommand" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "EnumCreatedEffectObjects" => Some(ctx.finish(DI_NOTATTACHED)?),
        "Escape" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "Poll" => Some(poll(ctx)?),
        "EnumEffectsInFile" => Some(ctx.finish(DI_NOTATTACHED)?),
        "WriteEffectToFile" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "BuildActionMap" | "SetActionMap" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        "GetImageInfo" => Some(ctx.finish(DIERR_UNSUPPORTED)?),
        _ => None,
    };
    Ok(result)
}

/// `HRESULT GetCapabilities(LPDIDEVCAPS)` — dinput.h:908-922.
///
/// `dwSize` is echoed back, so a guest compiled against the DX3 24-byte
/// `DIDEVCAPS` still sees a coherent struct: the FF fields WIE would fill are
/// simply absent from its `dwSize` and WIE does not write past it.
fn get_capabilities(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "GetCapabilities")?;
    let caps_va = read_arg(engine, ArgReg::Rdx, "GetCapabilities")?;

    if caps_va == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    let Some((class,)) = device_class(engine, &mut *ctx.state, this) else {
        return ctx.finish(DIERR_NOTFOUND);
    };

    // The guest's own sizeof, read back, so we never write past it.
    let size = read_u32(engine, caps_va).context("failed to read DIDEVCAPS::dwSize")?;
    // Below the DX3 24-byte floor the struct cannot even hold dwPOVs; refuse
    // rather than write a partial record.
    if size < 24 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }

    let caps = DiDevCaps {
        size,
        flags: DIDC_ATTACHED | DIDC_EMULATED | DIDC_POLLED,
        device_type: class.dev_type(),
        axes: class.axes(),
        buttons: class.buttons(),
        povs: 0,
        ff_sample_period: 0,
        ff_min_time_resolution: 0,
        firmware_revision: 0,
        hardware_revision: 0,
        ff_driver_version: 0,
    };
    // Write only what the guest asked for: `min(size, sizeof(DIDEVCAPS))`.
    with_typed_write::<DiDevCaps, _, _>(engine, caps_va, |slot| {
        *slot = caps;
        Ok(())
    })
    .context("failed to write DIDEVCAPS")?;
    ctx.finish(DI_OK)
}

/// `HRESULT EnumObjects(...)` — dinput.h:2000.
///
/// WIE models no individually enumerable objects: the keyboard's 256 key slots
/// and the mouse's axes + buttons are *positional* pseudo-objects inside the
/// data format (their identity is their `dwOfs`), not `DIDEVICEOBJECTINSTANCE`
/// records. So the guest's callback is never invoked and the call returns
/// `DI_OK` — the documented "enumeration finished, nothing matched" result.
/// A guest that needs per-object identity should read `DIDEVICEOBJECTDATA`'s
/// `dwOfs`, which `GetDeviceState` does fill in.
fn enum_objects(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "EnumObjects")?;
    let _callback = read_arg(engine, ArgReg::Rdx, "EnumObjects")?;
    let _context = read_arg(engine, ArgReg::R8, "EnumObjects")?;
    let _flags = read_arg(engine, ArgReg::R9, "EnumObjects")?;
    ctx.finish(DI_OK)
}

/// `HRESULT GetProperty(REFGUID, LPDIPROPHEADER)` — dinput.h:758-763.
///
/// Not implemented: WIE answers `DIERR_UNSUPPORTED` rather than writing a
/// zeroed `DIPROPDWORD` / `DIPROPRANGE` a guest would read as a real answer
/// (a guest that sees `lMin == lMax == 0` concludes the axis is dead, not that
/// the property is unavailable). `DIPROP_BUFFERSIZE` / `DIPROP_AXISMODE` are
/// the ones a DirectInput8 keyboard or mouse guest actually asks for, and they
/// are the first candidates when a real guest needs them: the axis-mode
/// question has a fixed answer here (absolute for the keyboard's keys,
/// relative for the mouse's X/Y) and the buffer-size one has none (this lane is
/// polled state, not a buffered ring).
fn get_property(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "GetProperty")?;
    let _guid = read_arg(engine, ArgReg::Rdx, "GetProperty")?;
    let prop_va = read_arg(engine, ArgReg::R9, "GetProperty")?;
    if prop_va == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    ctx.finish(DIERR_UNSUPPORTED)
}

/// `HRESULT Acquire()` / `Unacquire()` — recorded, not enforced.
///
/// WIE has one global keyboard and one global mouse, so there is nothing to
/// arbitrate: a guest may read state whether or not it acquired, and a second
/// guest cannot be locked out. `GetDeviceState` therefore never returns
/// `DIERR_NOTACQUIRED`.
fn acquire(ctx: &mut HandlerContext<'_>, acquire: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "Acquire")?;
    let state = &mut *ctx.state;
    match state.dinput8().device_mut(this) {
        Some(record) => {
            record.acquired = acquire;
            ctx.finish(DI_OK)
        }
        None => ctx.finish(DIERR_NOTFOUND),
    }
}

/// `HRESULT Poll()` — always `DI_OK`. WIE's devices are polled at read time
/// (`GetDeviceState` samples the mirror directly), so there is no buffer to
/// pump, and the documented contract for a polled device is that `Poll`
/// succeeding means "the state you read next is current" — which it is.
fn poll(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "Poll")?;
    let known = ctx.state.dinput8().device(this).is_some();
    ctx.finish(if known { DI_OK } else { DIERR_NOTFOUND })
}

/// `HRESULT SetDataFormat(LPCDIDATAFORMAT)` — dinput.h:734-741 plus the
/// `DIOBJECTDATAFORMAT` array at `rgodf` (offset 0x18 on Win64, because the
/// four `DWORD`s end at 0x14 and the pointer is 8-aligned).
///
/// The format is *copied* into host state, not retained as a guest pointer:
/// real DirectInput8 copies it too, and a guest is free to free its
/// `DIOBJECTDATAFORMAT` array as soon as this call returns.
fn set_data_format(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "SetDataFormat")?;
    let format_va = read_arg(engine, ArgReg::Rdx, "SetDataFormat")?;

    if format_va == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }

    let (object_size, data_size, num_objects, objects_va) =
        with_typed_read::<DiDataFormat, _, _>(engine, format_va, |format| {
            Ok((
                format.object_size,
                format.data_size,
                format.num_objects,
                format.objects,
            ))
        })
        .context("failed to read the guest DIDATAFORMAT")?;

    if num_objects == 0 || object_size == 0 || data_size == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    // A real DirectInput8 caps the object count; anything past this is a
    // malformed format, not a device we should try to honour.
    if num_objects > 0x1000 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }

    let mut objects = Vec::new();
    if objects_va != 0 {
        objects.reserve(usize::try_from(num_objects).unwrap_or(0));
        for index in 0..num_objects {
            let entry_va = objects_va
                .checked_add(u64::from(index).saturating_mul(
                    u64::try_from(std::mem::size_of::<DiObjectDataFormat>()).unwrap_or(24),
                ))
                .context("DIOBJECTDATAFORMAT address overflow")?;
            let entry = with_typed_read::<DiObjectDataFormat, _, _>(engine, entry_va, |o| Ok(*o))
                .context("failed to read a guest DIOBJECTDATAFORMAT")?;
            // Every object must live inside the report the guest declared, or
            // filling it would read past `dwDataSize`. Note this is *not*
            // `dwNumObjs * dwObjSize`: the standard mouse format declares 5
            // objects of 4 bytes in a 16-byte `DIMOUSESTATE`, because the
            // collection and the X axis share offset 0 and all four buttons
            // share one 4-byte slot.
            if entry
                .offset
                .checked_add(object_size)
                .is_none_or(|end| end > data_size)
            {
                return ctx.finish(DIERR_INVALIDPARAM);
            }
            objects.push(entry);
        }
    }

    let Some(record) = ctx.state.dinput8().device_mut(this) else {
        return ctx.finish(DIERR_NOTFOUND);
    };
    record.data_format = Some(GuestDataFormat { data_size, objects });
    ctx.finish(DI_OK)
}

/// `HRESULT SetCooperativeLevel(HWND, DWORD dwFlags)` — accepted and stored.
///
/// The `DISCL_*` flags are deliberately *not* enforced: there is one keyboard
/// and one mouse for the whole process, so exclusive/foreground/background
/// arbitration has nothing to arbitrate. A guest that sets
/// `DISCL_FOREGROUND | DISCL_EXCLUSIVE` gets a success it can rely on (its
/// input is not being stolen), which is the honest reading.
fn set_cooperative_level(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "SetCooperativeLevel")?;
    let hwnd = read_arg(engine, ArgReg::Rdx, "SetCooperativeLevel")?;
    let flags = read_arg(engine, ArgReg::R8, "SetCooperativeLevel")?;

    let Some(record) = ctx.state.dinput8().device_mut(this) else {
        return ctx.finish(DIERR_NOTFOUND);
    };
    record.cooperative_hwnd = hwnd;
    record.cooperative_flags = u32::try_from(flags).unwrap_or(u32::MAX);
    ctx.finish(DI_OK)
}

/// `HRESULT GetDeviceInfo(LPDIDEVICEINSTANCE)` — dinput.h:443-455.
fn get_device_info(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "GetDeviceInfo")?;
    let info_va = read_arg(engine, ArgReg::Rdx, "GetDeviceInfo")?;

    if info_va == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    let Some((class,)) = device_class(engine, &mut *ctx.state, this) else {
        return ctx.finish(DIERR_NOTFOUND);
    };
    write_device_instance(engine, info_va, class)?;
    ctx.finish(DI_OK)
}

/// Fill a `DIDEVICEINSTANCEA` (dinput.h:443-455, 580 bytes on Win64) with the
/// device's identity.
pub(super) fn write_device_instance(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
    class: DInputDeviceClass,
) -> Result<()> {
    let size = read_u32(engine, va).context("failed to read DIDEVICEINSTANCE::dwSize")?;
    with_typed_write::<DiDeviceInstance, _, _>(engine, va, |instance| {
        instance.size = size;
        instance.guid_instance = class.instance_guid();
        instance.guid_product = class.product_guid();
        instance.device_type = class.dev_type();
        write_ansi_field(&mut instance.instance_name, class.instance_name());
        write_ansi_field(&mut instance.product_name, class.product_name());
        instance.guid_ff_driver = [0_u8; 16];
        instance.usage_page = 0x01;
        instance.usage = match class {
            DInputDeviceClass::Keyboard => 0x06,
            DInputDeviceClass::Mouse => 0x02,
        };
        Ok(())
    })
    .context("failed to write DIDEVICEINSTANCEA")
}

/// NUL-terminated ASCII into a fixed `CHAR[MAX_PATH]` field, truncated.
fn write_ansi_field<const N: usize>(dst: &mut [u8; N], src: &str) {
    for (slot, byte) in dst.iter_mut().zip(src.as_bytes().iter().take(N - 1)) {
        *slot = *byte;
    }
}

/// `HRESULT GetDeviceState(DWORD cbData, LPVOID lpvData)` — dinput.h:2005.
///
/// Builds the device's raw report and republishes it as the
/// `DIDEVICEOBJECTDATA` array the `DIDF_CDATAFORMAT` flag asks for: one
/// 24-byte record per `DIOBJECTDATAFORMAT` the guest declared, carrying that
/// object's `dwOfs`, its `dwData` read out of the raw report **at that same
/// offset**, a common `dwTimeStamp`, and a per-report `dwSequence`.
///
/// The guest's own `DIDATAFORMAT` is the only source of truth for the report
/// shape — WIE hardcodes no offset of its own — so a guest that declares a
/// non-standard format still gets every object filled from the offset it asked
/// for.
///
/// Failure modes, all honest:
/// - unknown `this` → `DIERR_NOTFOUND`
/// - `SetDataFormat` never ran → `DIERR_NOTACQUIRED` (a device with no format
///   has nothing to report, and WIE will not invent a shape)
/// - `cbData` smaller than the format's `dwDataSize` → `DIERR_INVALIDPARAM`
fn get_device_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "GetDeviceState")?;
    let cb_data = u32::try_from(read_arg(engine, ArgReg::Rdx, "GetDeviceState")?).unwrap_or(0);
    let out_va = read_arg(engine, ArgReg::R8, "GetDeviceState")?;

    if out_va == 0 {
        return ctx.finish(DIERR_INVALIDPARAM);
    }
    if ctx.state.dinput8().device(this).is_none() {
        return ctx.finish(DIERR_NOTFOUND);
    }

    // Snapshot every big-lock reader *before* taking the device record, so the
    // keyboard drain / cursor read and the record update are disjoint borrows.
    let state = &mut *ctx.state;
    state.drain_key_writes();
    let keyboard: [u8; 256] = state
        .window_state()
        .keyboard_state
        .deref()
        .try_into()
        .unwrap_or([0_u8; 256]);
    let cursor = state.cursor_pos();

    let record = state
        .dinput8()
        .device_mut(this)
        .expect("device presence checked above");
    // Validate against the format with a scoped immutable borrow, then drop
    // it: `raw_report` needs `&mut record` to advance the mouse's cursor
    // baseline, and a rejected call must not have advanced it.
    let object_count = {
        let Some(format) = record.data_format.as_ref() else {
            return ctx.finish(DIERR_NOTACQUIRED);
        };
        if cb_data < format.data_size {
            return ctx.finish(DIERR_INVALIDPARAM);
        }
        format.objects.len()
    };

    let report = raw_report(record, &keyboard, cursor);
    let timestamp = u32::try_from(crate::kernel32::clock::tick_count_32()).unwrap_or(0);
    record.sequence = record.sequence.wrapping_add(1);
    let sequence = record.sequence;

    for index in 0..object_count {
        let Some(object) = record
            .data_format
            .as_ref()
            .and_then(|format| format.objects.get(index))
        else {
            break;
        };
        let entry_va = out_va
            .checked_add(
                u64::try_from(index)
                    .context("DIDEVICEOBJECTDATA index does not fit u64")?
                    .saturating_mul(u64::from(OBJECT_DATA_SIZE)),
            )
            .context("DIDEVICEOBJECTDATA address overflow")?;
        let data = DiDeviceObjectData {
            offset: object.offset,
            data: word_at(&report, object.offset),
            timestamp,
            sequence,
            app_data: 0,
        };
        with_typed_write::<DiDeviceObjectData, _, _>(engine, entry_va, |entry| {
            *entry = data;
            Ok(())
        })
        .context("failed to write DIDEVICEOBJECTDATA")?;
    }
    ctx.finish(DI_OK)
}

/// The little-endian `DWORD` at byte `offset` of `report`, or 0 when the
/// object lies outside the report WIE synthesized.
fn word_at(report: &[u8], offset: u32) -> u32 {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    report
        .get(start..start.saturating_add(4))
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

/// The device's raw report bytes, straight from the live input state.
///
/// Two shapes, both taken from the header rather than invented:
///
/// - **Keyboard** — 256 consecutive `DWORD` slots (`c_dfDIKeyboard`'s shape:
///   one slot per DIK code, 4-byte stride, 1024 bytes). The `DIK_*`
///   constants are deliberately equal to the `VK_*` codes for the standard set
///   (dinput.h:510-640), so slot *i* is a raw `KeyboardState[i]` pass-through:
///   the documented scope is no axis or button remapping.
/// - **Mouse** — `DIMOUSESTATE2` (dinput.h:2123-2128, 20 bytes: `lX` @0x00,
///   `lY` @0x04, `lZ` @0x08, `rgbButtons[8]` @0x0C). The 8-button shape is
///   always built because its first 16 bytes are byte-identical to
///   `DIMOUSESTATE` (dinput.h:2114-2119), so a guest on either standard
///   format reads a correct prefix.
///
/// The X/Y axes are *relative* in a DirectInput mouse report, but WIE's host
/// input seam only publishes an absolute cursor position, so the delta is
/// differenced against this device's previous report and the baseline is
/// advanced here — which is exactly the consumed-on-read semantics a
/// relative axis has.
fn raw_report(
    record: &mut DInputDeviceRecord,
    keyboard: &[u8; 256],
    cursor: Option<(i32, i32)>,
) -> Vec<u8> {
    match record.class {
        DInputDeviceClass::Keyboard => {
            let mut report = Vec::with_capacity(256 * 4);
            for slot in keyboard.iter() {
                report.extend_from_slice(&u32::from(*slot).to_le_bytes());
            }
            report
        }
        DInputDeviceClass::Mouse => {
            let (x, y) = match (cursor, record.last_cursor) {
                (Some(position), Some((last_x, last_y))) => {
                    (position.0 - last_x, position.1 - last_y)
                }
                // First report: there is no previous position, so the honest
                // delta is zero rather than the absolute cursor position.
                (Some(_), None) => (0, 0),
                (None, _) => (0, 0),
            };
            record.last_cursor = cursor;
            let state = DiMouseState2 {
                x,
                y,
                // The wheel: WIE's host seam carries no scroll state.
                z: 0,
                buttons: mouse_buttons(keyboard),
            };
            state.as_bytes().to_vec()
        }
    }
}

/// The mouse button bytes for a `DIMOUSESTATE` report.
///
/// WIE's host input seam pushes keyboard virtual keys and the cursor position
/// only — `VK_LBUTTON` (0x01) and friends are never written — so every button
/// truthfully reads "up". Isolated in one function so the change that makes
/// buttons real is a single obvious edit once the host seam carries button
/// state.
fn mouse_buttons(keyboard: &[u8; 256]) -> [u8; 8] {
    let mut buttons = [0_u8; 8];
    // VK_LBUTTON 0x01, VK_RBUTTON 0x02, VK_MBUTTON 0x04, VK_XBUTTON1 0x05,
    // VK_XBUTTON2 0x06.
    const MOUSE_VKS: [usize; 5] = [0x01, 0x02, 0x04, 0x05, 0x06];
    for (index, vk) in MOUSE_VKS.into_iter().enumerate() {
        let down = keyboard.get(vk).copied().unwrap_or(0) & 0x80;
        if let Some(slot) = buttons.get_mut(index) {
            *slot = if down == 0 { 0 } else { 0x80 };
        }
    }
    buttons
}

/// `(class,)` for the device at guest object address `this`.
fn device_class(
    _engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    this: u64,
) -> Option<(DInputDeviceClass,)> {
    state.dinput8().device(this).map(|record| (record.class,))
}
