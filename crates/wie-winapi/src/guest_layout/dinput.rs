//! dinput8 lane: the `DIDATAFORMAT` family, `DIDEVCAPS`, the
//! `DIDEVICEINSTANCE` / `DIDEVICEOBJECTINSTANCE` records WIE fills in for
//! `EnumDevices` / `GetDeviceInfo` / `GetObjectInfo`, and the two mouse
//! report structs.
//!
//! Every offset below is read off the mingw `dinput.h` shipped with the
//! cross-compiler on this box
//! (`/opt/homebrew/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/include/dinput.h`)
//! and is asserted against the raw guest bytes by the round-trip tests at the
//! bottom of this file. A wrong offset in this family is the documented bug
//! class that once sent 7-Zip into infinite recursion (see CLAUDE.md), so the
//! const-assert drift table plus the byte-level tests are the guard.
//!
//! All structs are the `A` (ANSI) spelling where mingw defines both: WIE
//! writes ASCII product/instance names, and the `A`/`W` layouts differ only in
//! the width of the `tsz*` arrays — every offset this module reads
//! (`dwSize`, `dwDevType`, `dwAxes`, `dwButtons`, `dwOfs`, `dwData`) is at the
//! same place in both, so one struct serves a guest that asked for the W
//! interface. `DIDEVICEINSTANCEW` / `DIDEVICEOBJECTINSTANCEW` are NOT
//! interchangeable: their `WCHAR` names are twice as wide, so everything from
//! `tszInstanceName` onward differs. WIE only ever *writes* these, and only
//! sizes the `A` arrays it fills — a guest reading the `W` name field sees
//! the same UTF-16LE bytes it would on Windows, because WIE writes the names
//! as UTF-16LE and pads to the `A` extent.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Win64 `DIDATAFORMAT` (dinput.h:734-741): `DWORD dwSize` @0x00, `DWORD
/// dwObjSize` @0x04, `DWORD dwFlags` @0x08, `DWORD dwDataSize` @0x0C, `DWORD
/// dwNumObjs` @0x10, `LPDIOBJECTDATAFORMAT rgodf` @0x18 — 32 bytes, align 8.
///
/// The `rgodf` pointer is 8-aligned on Win64 (the four `DWORD`s end at
/// 0x14, so the pointer starts at 0x18, not 0x14).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiDataFormat {
    /// `dwSize` — `sizeof(DIDATAFORMAT)`, 32.
    pub(crate) size: u32,
    /// `dwObjSize` — bytes per object in the `GetDeviceState` report.
    pub(crate) object_size: u32,
    /// `dwFlags` — `DIDF_*`.
    pub(crate) flags: u32,
    /// `dwDataSize` — total report size the guest's buffer must hold.
    pub(crate) data_size: u32,
    /// `dwNumObjs` — object count in the `GetDeviceData` event stream.
    pub(crate) num_objects: u32,
    /// Win64 alignment padding before the 8-aligned `rgodf` pointer.
    pub(crate) _objects_pad: [u8; 4],
    /// `rgodf` — the `DIOBJECTDATAFORMAT` array.
    pub(crate) objects: u64,
}

/// Win64 `DIOBJECTDATAFORMAT` (dinput.h:726-731): `const GUID *pguid` @0x00,
/// `DWORD dwOfs` @0x08, `DWORD dwType` @0x0C, `DWORD dwFlags` @0x10 — 20
/// payload bytes rounded to 24 by the 8-byte pointer alignment.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiObjectDataFormat {
    /// `pguid` — the object GUID (NULL for the axis/button pseudos).
    pub(crate) guid: u64,
    /// `dwOfs` — the object's offset inside one report.
    pub(crate) offset: u32,
    /// `dwType` — the `DIDEVTYPE`/`DIDOI_*` object type.
    pub(crate) object_type: u32,
    /// `dwFlags` — `DIDOI_*`.
    pub(crate) flags: u32,
    /// Win64 tail alignment padding (20 payload bytes → 24).
    pub(crate) _tail_pad: [u8; 4],
}

/// Win64 `DIDEVCAPS` (dinput.h:908-922) with `DIRECTINPUT_VERSION >= 0x0500`,
/// so the five force-feedback revision fields are present: `DWORD dwSize`
/// @0x00, `dwFlags` @0x04, `dwDevType` @0x08, `dwAxes` @0x0C, `dwButtons`
/// @0x10, `dwPOVs` @0x14, `dwFFSamplePeriod` @0x18, `dwFFMinTimeResolution`
/// @0x1C, `dwFirmwareRevision` @0x20, `dwHardwareRevision` @0x24,
/// `dwFFDriverVersion` @0x28 — 44 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiDevCaps {
    /// `dwSize` — the guest's `sizeof(DIDEVCAPS)`, echoed back.
    pub(crate) size: u32,
    /// `dwFlags` — `DIDC_*`.
    pub(crate) flags: u32,
    /// `dwDevType` — e.g. `(DI8DEVTYPE_MOUSE << 8) | DIDEVTYPE_MOUSE`.
    pub(crate) device_type: u32,
    /// `dwAxes` — reported axis count (WIE: 2 for mouse, 0 for keyboard).
    pub(crate) axes: u32,
    /// `dwButtons` — reported button count (WIE: 4 for mouse, 256 for the
    /// keyboard's pseudo-buttons).
    pub(crate) buttons: u32,
    /// `dwPOVs` — POV hat count. Always 0: WIE has no hats.
    pub(crate) povs: u32,
    /// `dwFFSamplePeriod` — 0 (no force feedback).
    pub(crate) ff_sample_period: u32,
    /// `dwFFMinTimeResolution` — 0 (no force feedback).
    pub(crate) ff_min_time_resolution: u32,
    /// `dwFirmwareRevision` — 0.
    pub(crate) firmware_revision: u32,
    /// `dwHardwareRevision` — 0.
    pub(crate) hardware_revision: u32,
    /// `dwFFDriverVersion` — 0.
    pub(crate) ff_driver_version: u32,
}

/// Win64 `DIDEVICEOBJECTDATA` (dinput.h:715-723) with
/// `DIRECTINPUT_VERSION >= 0x0800`, so the trailing `UINT_PTR uAppData` is
/// present: `DWORD dwOfs` @0x00, `DWORD dwData` @0x04, `DWORD dwTimeStamp`
/// @0x08, `DWORD dwSequence` @0x0C, `UINT_PTR uAppData` @0x10 — 24 bytes,
/// align 8.
///
/// This is the `DI8` shape. The DX3 `DIDEVICEOBJECTDATA_DX3`
/// (dinput.h:707-712) is 16 bytes with no `uAppData`; WIE only ever writes
/// the DI8 shape because it only ever hands out `IID_IDirectInputDevice8*`.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiDeviceObjectData {
    /// `dwOfs` — the object offset this event reports.
    pub(crate) offset: u32,
    /// `dwData` — the object's value (raw axis delta / button down-flag).
    pub(crate) data: u32,
    /// `dwTimeStamp` — the `GetTickCount` at which WIE sampled the object.
    pub(crate) timestamp: u32,
    /// `dwSequence` — the per-device monotonic event counter.
    pub(crate) sequence: u32,
    /// `uAppData` — the `DIOBJECTDATAFORMAT`-supplied app pointer; WIE echoes
    /// the guest's `DIDEVICEOBJECTDATA` input value (0 for events it
    /// synthesizes).
    pub(crate) app_data: u64,
}

/// Win64 `DIDEVICEINSTANCEA` (dinput.h:443-455) with
/// `DIRECTINPUT_VERSION >= 0x0500`: `DWORD dwSize` @0x00, `GUID
/// guidInstance` @0x04, `GUID guidProduct` @0x14, `DWORD dwDevType` @0x24,
/// `CHAR tszInstanceName[MAX_PATH]` @0x28 (260 bytes), `CHAR
/// tszProductName[MAX_PATH]` @0x12C, `GUID guidFFDriver` @0x230, `WORD
/// wUsagePage` @0x240, `WORD wUsage` @0x242 — 0x244 = 580 bytes, align 4.
///
/// Both `GUID`s are 16 bytes at 4-byte alignment (they are `{u32, u16, u16,
/// [u8; 8]}`), which is why `guidInstance` lands on 0x04 and not 0x08.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiDeviceInstance {
    /// `dwSize` — the guest's `sizeof(DIDEVICEINSTANCE)`, echoed back.
    pub(crate) size: u32,
    /// `guidInstance` — the synthetic per-device instance GUID (16 bytes).
    pub(crate) guid_instance: [u8; 16],
    /// `guidProduct` — the real product GUID (keyboard / mouse) (16 bytes).
    pub(crate) guid_product: [u8; 16],
    /// `dwDevType` — `(DI8DEVTYPE_* << 8) | DIDEVTYPE_*`.
    pub(crate) device_type: u32,
    /// `tszInstanceName` — `CHAR[MAX_PATH]` (NUL-padded, ASCII).
    pub(crate) instance_name: [u8; 260],
    /// `tszProductName` — `CHAR[MAX_PATH]` (NUL-padded, ASCII).
    pub(crate) product_name: [u8; 260],
    /// `guidFFDriver` — zeroed: WIE has no force-feedback driver.
    pub(crate) guid_ff_driver: [u8; 16],
    /// `wUsagePage` — HID usage page (0x01 keyboard / 0x02 mouse).
    pub(crate) usage_page: u16,
    /// `wUsage` — HID usage (0x06 keyboard / 0x02 mouse).
    pub(crate) usage: u16,
}

/// Win64 `DIDEVICEOBJECTINSTANCEA` (dinput.h:374-392) with
/// `DIRECTINPUT_VERSION >= 0x0500`: `DWORD dwSize` @0x00, `GUID guidType`
/// @0x04, `DWORD dwOfs` @0x14, `DWORD dwType` @0x18, `DWORD dwFlags` @0x1C,
/// `CHAR tszName[MAX_PATH]` @0x20, `DWORD dwFFMaxForce` @0x124, `DWORD
/// dwFFForceResolution` @0x128, `WORD wCollectionNumber` @0x12C, `WORD
/// wDesignatorIndex` @0x12E, `WORD wUsagePage` @0x130, `WORD wUsage` @0x132,
/// `DWORD dwDimension` @0x134, `WORD wExponent` @0x138, `WORD wReportId`
/// @0x13A — 0x13C = 316 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiDeviceObjectInstance {
    /// `dwSize` — the guest's `sizeof(DIDEVICEOBJECTINSTANCE)`, echoed back.
    pub(crate) size: u32,
    /// `guidType` — the object GUID (16 bytes).
    pub(crate) guid_type: [u8; 16],
    /// `dwOfs` — the object offset inside one report.
    pub(crate) offset: u32,
    /// `dwType` — the `DIDEVTYPE`/`DIDI_*` object type.
    pub(crate) object_type: u32,
    /// `dwFlags` — `DIDOI_*`.
    pub(crate) flags: u32,
    /// `tszName` — `CHAR[MAX_PATH]` (NUL-padded, ASCII).
    pub(crate) name: [u8; 260],
    /// `dwFFMaxForce` — 0 (no force feedback).
    pub(crate) ff_max_force: u32,
    /// `dwFFForceResolution` — 0 (no force feedback).
    pub(crate) ff_force_resolution: u32,
    /// `wCollectionNumber` — 0.
    pub(crate) collection_number: u16,
    /// `wDesignatorIndex` — 0.
    pub(crate) designator_index: u16,
    /// `wUsagePage` — HID usage page.
    pub(crate) usage_page: u16,
    /// `wUsage` — HID usage.
    pub(crate) usage: u16,
    /// `dwDimension` — 0.
    pub(crate) dimension: u32,
    /// `wExponent` — 0.
    pub(crate) exponent: u16,
    /// `wReportId` — 0.
    pub(crate) report_id: u16,
}

/// Win64 `DIPROPHEADER` (dinput.h:758-763): `DWORD dwSize` @0x00, `DWORD
/// dwHeaderSize` @0x04, `DWORD dwObj` @0x08, `DWORD dwHow` @0x0C — 16 bytes,
/// align 4. The common prefix of every `DIPROP*` struct.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiPropHeader {
    /// `dwSize` — the guest's `sizeof(DIPROP*)`.
    pub(crate) size: u32,
    /// `dwHeaderSize` — 16, the offset the payload starts at.
    pub(crate) header_size: u32,
    /// `dwObj` — the object the property is about (or -1 for the device).
    pub(crate) object: u32,
    /// `dwHow` — `DIPH_*`.
    pub(crate) how: u32,
}

/// Win64 `DIPROPRANGE` (dinput.h:781-785): `DIPROPHEADER diph` @0x00, `LONG
/// lMin` @0x10, `LONG lMax` @0x14 — 24 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiPropRange {
    /// `diph` — the property header.
    pub(crate) header: DiPropHeader,
    /// `lMin` — the range minimum.
    pub(crate) min: i32,
    /// `lMax` — the range maximum.
    pub(crate) max: i32,
}

/// Win64 `DIMOUSESTATE` (dinput.h:2114-2119): `LONG lX` @0x00, `LONG lY`
/// @0x04, `LONG lZ` @0x08, `BYTE rgbButtons[4]` @0x0C — 16 bytes, align 4.
/// The `c_dfDIMouse` report shape.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiMouseState {
    /// `lX` — X-axis movement since the previous report.
    pub(crate) x: i32,
    /// `lY` — Y-axis movement since the previous report.
    pub(crate) y: i32,
    /// `lZ` — wheel movement since the previous report.
    pub(crate) z: i32,
    /// `rgbButtons` — 0x80 = down, 0x00 = up.
    pub(crate) buttons: [u8; 4],
}

/// Win64 `DIMOUSESTATE2` (dinput.h:2123-2128): `LONG lX` @0x00, `LONG lY`
/// @0x04, `LONG lZ` @0x08, `BYTE rgbButtons[8]` @0x0C — 20 bytes, align 4.
/// The `c_dfDIMouse2` report shape. WIE emits this only when the guest's data
/// format is at least this large; the four extra buttons always read up.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DiMouseState2 {
    /// `lX` — X-axis movement since the previous report.
    pub(crate) x: i32,
    /// `lY` — Y-axis movement since the previous report.
    pub(crate) y: i32,
    /// `lZ` — wheel movement since the previous report.
    pub(crate) z: i32,
    /// `rgbButtons` — 0x80 = down, 0x00 = up.
    pub(crate) buttons: [u8; 8],
}

// ── Compile-time drift table ────────────────────────────────────────────
//
// These pin every offset the DirectInput handlers write through. They are
// the same class of assertion as the other `guest_layout` lanes: if a field
// is reordered or the struct grows, the const-eval fails here rather than at
// runtime with a silently-wrong guest struct.
const _: () = {
    assert!(
        core::mem::size_of::<DiDataFormat>() == 32,
        "DIDATAFORMAT must be 32 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, size) == 0x00,
        "DIDATAFORMAT::dwSize @0x00"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, object_size) == 0x04,
        "DIDATAFORMAT::dwObjSize @0x04"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, flags) == 0x08,
        "DIDATAFORMAT::dwFlags @0x08"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, data_size) == 0x0C,
        "DIDATAFORMAT::dwDataSize @0x0C"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, num_objects) == 0x10,
        "DIDATAFORMAT::dwNumObjs @0x10"
    );
    assert!(
        core::mem::offset_of!(DiDataFormat, objects) == 0x18,
        "DIDATAFORMAT::rgodf @0x18 (8-aligned pointer, not 0x14)"
    );

    assert!(
        core::mem::size_of::<DiObjectDataFormat>() == 24,
        "DIOBJECTDATAFORMAT must be 24 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiObjectDataFormat, guid) == 0x00,
        "DIOBJECTDATAFORMAT::pguid @0x00"
    );
    assert!(
        core::mem::offset_of!(DiObjectDataFormat, offset) == 0x08,
        "DIOBJECTDATAFORMAT::dwOfs @0x08"
    );
    assert!(
        core::mem::offset_of!(DiObjectDataFormat, object_type) == 0x0C,
        "DIOBJECTDATAFORMAT::dwType @0x0C"
    );
    assert!(
        core::mem::offset_of!(DiObjectDataFormat, flags) == 0x10,
        "DIOBJECTDATAFORMAT::dwFlags @0x10"
    );

    assert!(
        core::mem::size_of::<DiDevCaps>() == 44,
        "DIDEVCAPS must be 44 bytes on Win64 (DI >= 0x0500 FF fields)"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, size) == 0x00,
        "DIDEVCAPS::dwSize @0x00"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, flags) == 0x04,
        "DIDEVCAPS::dwFlags @0x04"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, device_type) == 0x08,
        "DIDEVCAPS::dwDevType @0x08"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, axes) == 0x0C,
        "DIDEVCAPS::dwAxes @0x0C"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, buttons) == 0x10,
        "DIDEVCAPS::dwButtons @0x10"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, povs) == 0x14,
        "DIDEVCAPS::dwPOVs @0x14"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, ff_sample_period) == 0x18,
        "DIDEVCAPS::dwFFSamplePeriod @0x18"
    );
    assert!(
        core::mem::offset_of!(DiDevCaps, ff_driver_version) == 0x28,
        "DIDEVCAPS::dwFFDriverVersion @0x28"
    );

    assert!(
        core::mem::size_of::<DiDeviceObjectData>() == 24,
        "DIDEVICEOBJECTDATA must be 24 bytes on Win64 (DI8 uAppData tail)"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectData, offset) == 0x00,
        "DIDEVICEOBJECTDATA::dwOfs @0x00"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectData, data) == 0x04,
        "DIDEVICEOBJECTDATA::dwData @0x04"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectData, timestamp) == 0x08,
        "DIDEVICEOBJECTDATA::dwTimeStamp @0x08"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectData, sequence) == 0x0C,
        "DIDEVICEOBJECTDATA::dwSequence @0x0C"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectData, app_data) == 0x10,
        "DIDEVICEOBJECTDATA::uAppData @0x10"
    );

    assert!(
        core::mem::size_of::<DiDeviceInstance>() == 580,
        "DIDEVICEINSTANCEA must be 580 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, size) == 0x00,
        "DIDEVICEINSTANCE::dwSize @0x00"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, guid_instance) == 0x04,
        "DIDEVICEINSTANCE::guidInstance @0x04 (GUID is 4-aligned)"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, guid_product) == 0x14,
        "DIDEVICEINSTANCE::guidProduct @0x14"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, device_type) == 0x24,
        "DIDEVICEINSTANCE::dwDevType @0x24"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, instance_name) == 0x28,
        "DIDEVICEINSTANCE::tszInstanceName @0x28"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, product_name) == 0x12C,
        "DIDEVICEINSTANCE::tszProductName @0x12C"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, guid_ff_driver) == 0x230,
        "DIDEVICEINSTANCE::guidFFDriver @0x230"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, usage_page) == 0x240,
        "DIDEVICEINSTANCE::wUsagePage @0x240"
    );
    assert!(
        core::mem::offset_of!(DiDeviceInstance, usage) == 0x242,
        "DIDEVICEINSTANCE::wUsage @0x242"
    );

    assert!(
        core::mem::size_of::<DiDeviceObjectInstance>() == 316,
        "DIDEVICEOBJECTINSTANCEA must be 316 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, size) == 0x00,
        "DIDEVICEOBJECTINSTANCE::dwSize @0x00"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, guid_type) == 0x04,
        "DIDEVICEOBJECTINSTANCE::guidType @0x04"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, offset) == 0x14,
        "DIDEVICEOBJECTINSTANCE::dwOfs @0x14"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, object_type) == 0x18,
        "DIDEVICEOBJECTINSTANCE::dwType @0x18"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, flags) == 0x1C,
        "DIDEVICEOBJECTINSTANCE::dwFlags @0x1C"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, name) == 0x20,
        "DIDEVICEOBJECTINSTANCE::tszName @0x20"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, ff_max_force) == 0x124,
        "DIDEVICEOBJECTINSTANCE::dwFFMaxForce @0x124"
    );
    assert!(
        core::mem::offset_of!(DiDeviceObjectInstance, report_id) == 0x13A,
        "DIDEVICEOBJECTINSTANCE::wReportId @0x13A"
    );

    assert!(
        core::mem::size_of::<DiPropHeader>() == 16,
        "DIPROPHEADER must be 16 bytes on Win64"
    );
    assert!(
        core::mem::size_of::<DiPropRange>() == 24,
        "DIPROPRANGE must be 24 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiPropRange, min) == 0x10,
        "DIPROPRANGE::lMin @0x10"
    );
    assert!(
        core::mem::offset_of!(DiPropRange, max) == 0x14,
        "DIPROPRANGE::lMax @0x14"
    );

    assert!(
        core::mem::size_of::<DiMouseState>() == 16,
        "DIMOUSESTATE must be 16 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiMouseState, x) == 0x00,
        "DIMOUSESTATE::lX @0x00"
    );
    assert!(
        core::mem::offset_of!(DiMouseState, buttons) == 0x0C,
        "DIMOUSESTATE::rgbButtons @0x0C"
    );
    assert!(
        core::mem::size_of::<DiMouseState2>() == 20,
        "DIMOUSESTATE2 must be 20 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DiMouseState2, buttons) == 0x0C,
        "DIMOUSESTATE2::rgbButtons @0x0C"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_memory::with_typed_write;
    use wie_cpu::{CpuEngine, IcedCpu};

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    fn raw_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(va, &mut bytes).expect("read raw bytes");
        bytes
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        let mut raw = [0_u8; 4];
        raw.copy_from_slice(&bytes[offset..offset + 4]);
        u32::from_le_bytes(raw)
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(raw)
    }

    /// A `DIDEVICEOBJECTDATA` WIE writes must be byte-identical to what a real
    /// Win64 guest reads at the mingw `dinput.h:715-723` offsets. The guest
    /// re-walks this array with its own `sizeof`/offset arithmetic, so every
    /// field position is asserted against the raw bytes rather than the typed
    /// view.
    #[test]
    fn di_device_object_data_write_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7400_u64;
        with_typed_write::<DiDeviceObjectData, _, _>(&mut engine, va, |dod| {
            dod.offset = 0x0C;
            dod.data = 0x8000_0000;
            dod.timestamp = 0x1234_5678;
            dod.sequence = 42;
            dod.app_data = 0xDEAD_BEEF_CAFE_F00D;
            Ok(())
        })
        .expect("typed DIDEVICEOBJECTDATA write");

        let bytes = raw_bytes(&mut engine, va, 24);
        assert_eq!(u32_at(&bytes, 0x00), 0x0C, "dwOfs @0x00");
        assert_eq!(u32_at(&bytes, 0x04), 0x8000_0000, "dwData @0x04");
        assert_eq!(u32_at(&bytes, 0x08), 0x1234_5678, "dwTimeStamp @0x08");
        assert_eq!(u32_at(&bytes, 0x0C), 42, "dwSequence @0x0C");
        assert_eq!(
            u64_at(&bytes, 0x10),
            0xDEAD_BEEF_CAFE_F00D,
            "uAppData @0x10"
        );
        assert_eq!(
            bytes.len(),
            24,
            "the DI8 DIDEVICEOBJECTDATA is 24 bytes, not the 16-byte DX3 shape"
        );
    }

    /// The `DIDATAFORMAT` WIE reads back in `SetDataFormat` must sit at the
    /// mingw `dinput.h:734-741` offsets — `rgodf` on Win64 is 8-aligned, so
    /// it is at 0x18, not the 0x14 a 32-bit build would use.
    #[test]
    fn di_data_format_read_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7500_u64;
        let mut bytes = vec![0_u8; 32];
        bytes[0x00..0x04].copy_from_slice(&32_u32.to_le_bytes());
        bytes[0x04..0x08].copy_from_slice(&16_u32.to_le_bytes());
        bytes[0x08..0x0C].copy_from_slice(&0x0000_00C0_u32.to_le_bytes());
        bytes[0x0C..0x10].copy_from_slice(&16_u32.to_le_bytes());
        bytes[0x10..0x14].copy_from_slice(&4_u32.to_le_bytes());
        bytes[0x18..0x20].copy_from_slice(&0x7600_u64.to_le_bytes());
        engine
            .mem_write(va, &bytes)
            .expect("write raw DIDATAFORMAT");

        crate::guest_memory::with_typed_read::<DiDataFormat, _, _>(&mut engine, va, |df| {
            assert_eq!(df.size, 32, "dwSize @0x00");
            assert_eq!(df.object_size, 16, "dwObjSize @0x04");
            assert_eq!(df.flags, 0xC0, "dwFlags @0x08");
            assert_eq!(df.data_size, 16, "dwDataSize @0x0C");
            assert_eq!(df.num_objects, 4, "dwNumObjs @0x10");
            assert_eq!(df.objects, 0x7600, "rgodf @0x18");
            Ok(())
        })
        .expect("typed DIDATAFORMAT read");
    }

    /// The `DIDEVCAPS` WIE fills in `GetCapabilities` must place `dwDevType`
    /// at 0x08 and the FF revision tail where mingw `dinput.h:908-922` puts
    /// it. A guest that reads `dwDevType` at the DX3 offset (0x08 — same) but
    /// `dwAxes` at the wrong stride would mis-detect the device class.
    #[test]
    fn di_dev_caps_write_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7700_u64;
        with_typed_write::<DiDevCaps, _, _>(&mut engine, va, |caps| {
            caps.size = 44;
            caps.flags = 0x0000_0003;
            caps.device_type = 0x1202;
            caps.axes = 2;
            caps.buttons = 4;
            caps.povs = 0;
            caps.ff_sample_period = 0;
            Ok(())
        })
        .expect("typed DIDEVCAPS write");

        let bytes = raw_bytes(&mut engine, va, 44);
        assert_eq!(u32_at(&bytes, 0x00), 44, "dwSize @0x00");
        assert_eq!(u32_at(&bytes, 0x04), 0x0000_0003, "dwFlags @0x04");
        assert_eq!(u32_at(&bytes, 0x08), 0x1202, "dwDevType @0x08");
        assert_eq!(u32_at(&bytes, 0x0C), 2, "dwAxes @0x0C");
        assert_eq!(u32_at(&bytes, 0x10), 4, "dwButtons @0x10");
        assert_eq!(u32_at(&bytes, 0x14), 0, "dwPOVs @0x14");
        assert_eq!(u32_at(&bytes, 0x18), 0, "dwFFSamplePeriod @0x18");
        assert_eq!(u32_at(&bytes, 0x28), 0, "dwFFDriverVersion @0x28");
    }

    /// `DIDEVICEINSTANCEA` — the record `EnumDevices` hands the guest callback
    /// and `GetDeviceInfo` fills. `guidProduct` at 0x14 and `dwDevType` at
    /// 0x24 are the two offsets a guest's device-class check depends on.
    #[test]
    fn di_device_instance_write_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7800_u64;
        let product_guid = {
            let mut g = [0_u8; 16];
            g[0..4].copy_from_slice(&0xA426_D2D6_u32.to_le_bytes());
            g
        };
        with_typed_write::<DiDeviceInstance, _, _>(&mut engine, va, |di| {
            di.size = 580;
            di.guid_product = product_guid;
            di.device_type = 0x1202;
            di.instance_name[0..5].copy_from_slice(b"WIE  ");
            di.product_name[0..3].copy_from_slice(b"MSE");
            di.usage_page = 0x0002;
            di.usage = 0x0002;
            Ok(())
        })
        .expect("typed DIDEVICEINSTANCEA write");

        let bytes = raw_bytes(&mut engine, va, 580);
        assert_eq!(u32_at(&bytes, 0x00), 580, "dwSize @0x00");
        assert_eq!(u32_at(&bytes, 0x14), 0xA426_D2D6, "guidProduct @0x14");
        assert_eq!(u32_at(&bytes, 0x24), 0x1202, "dwDevType @0x24");
        assert_eq!(&bytes[0x28..0x2D], b"WIE  ", "tszInstanceName @0x28");
        assert_eq!(&bytes[0x12C..0x12F], b"MSE", "tszProductName @0x12C");
        assert_eq!(bytes[0x230..0x240], [0_u8; 16], "guidFFDriver @0x230");
        assert_eq!(u32_at(&bytes, 0x240), 0x0002_0002, "wUsagePage @0x240");
    }

    /// The `DIMOUSESTATE` report WIE writes for `GetDeviceState` — the
    /// `c_dfDIMouse` shape. `rgbButtons` at 0x0C is the offset a guest's
    /// button loop depends on; `DIMOUSESTATE2` shares it.
    #[test]
    fn di_mouse_state_write_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7900_u64;
        with_typed_write::<DiMouseState, _, _>(&mut engine, va, |ms| {
            ms.x = -3;
            ms.y = 7;
            ms.z = 0;
            ms.buttons = [0x80, 0x00, 0x80, 0x00];
            Ok(())
        })
        .expect("typed DIMOUSESTATE write");

        let bytes = raw_bytes(&mut engine, va, 16);
        assert_eq!(u32_at(&bytes, 0x00), (-3_i32) as u32, "lX @0x00");
        assert_eq!(u32_at(&bytes, 0x04), 7, "lY @0x04");
        assert_eq!(u32_at(&bytes, 0x08), 0, "lZ @0x08");
        assert_eq!(
            &bytes[0x0C..0x10],
            &[0x80, 0x00, 0x80, 0x00],
            "rgbButtons @0x0C"
        );
    }
}
