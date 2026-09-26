//! DirectInput8 (`dinput8.dll`) handler tests: the DLL wiring, the guest COM
//! vtables, the synthesized `DIDEVICEOBJECTDATA` reports, the two-device
//! `EnumDevices` walk, and the honest-failure surface.
//!
//! Everything a guest reads is asserted against **raw guest bytes** at the
//! mingw `dinput.h` offsets, never through a typed view alone: a wrong struct
//! offset in this family is the documented bug class that once sent 7-Zip
//! into infinite recursion (see CLAUDE.md), and the guest re-walks these
//! arrays with its own `sizeof` arithmetic.

use super::*;
use crate::dinput::DInputDeviceClass;
use crate::dispatch_table::{dispatch_winapi, is_winapi_implemented, is_winapi_library};
use crate::fake_va::{
    DInput8Iface, DirectInput8Method, DirectInputDevice8Method, encode_com_dinput8,
};
use crate::guest_layout::{DiDataFormat, DiObjectDataFormat};
use crate::guest_memory::with_typed_write;
use crate::{OuterReturn, WinApiControlSignal};

// ── Harness ─────────────────────────────────────────────────────────────

/// A `DIDATAFORMAT` + `DIOBJECTDATAFORMAT` array in guest memory, laid out the
/// way a real guest's static `c_dfDIKeyboard` copy is: the format struct at
/// `FORMAT_VA`, the object array right after it.
const FORMAT_VA: u64 = 0xA000;
const OBJECTS_VA: u64 = 0xB000;

/// The `c_dfDIKeyboard` object array: 256 slots of 4 bytes, DIK code *i* at
/// `dwOfs = i * 4`, in order. That is the whole keyboard report — the guest's
/// own array is the only place the shape is written down, so a test that
/// declares it here is testing the same contract a real guest relies on.
fn write_keyboard_format(engine: &mut IcedCpu, num_objects: u32) {
    let data_size = num_objects.saturating_mul(4);
    with_typed_write::<DiDataFormat, _, _>(engine, FORMAT_VA, |format| {
        format.size = 32;
        format.object_size = 4;
        // DIDF_CDATAFORMAT (0x0000_0001) | DIDF_ABSAXIS (0x1) | DIDF_RELAXIS (0x2)
        format.flags = 0x0000_0003;
        format.data_size = data_size;
        format.num_objects = num_objects;
        format.objects = OBJECTS_VA;
        Ok(())
    })
    .expect("write the guest DIDATAFORMAT");
    for index in 0..num_objects {
        let va = OBJECTS_VA + u64::from(index) * 24;
        with_typed_write::<DiObjectDataFormat, _, _>(engine, va, |object| {
            object.guid = 0;
            object.offset = index * 4;
            object.object_type = 0;
            object.flags = 0;
            Ok(())
        })
        .expect("write a guest DIOBJECTDATAFORMAT");
    }
}

/// The `c_dfDIMouse` object array: the five standard entries at the
/// `DIMOUSESTATE` offsets from dinput.h:2114-2119 (`lX` @0x00, `lY` @0x04,
/// `lZ` @0x08, `rgbButtons` @0x0C). `DIMOUSESTATE2` shares those four offsets
/// and only widens `rgbButtons`, so one array covers both standard formats.
fn write_mouse_format(engine: &mut IcedCpu) {
    // (dwOfs, is_collection)
    const ENTRIES: [(u32, u32); 5] = [
        (0x00, 0x8000_0000), // collection
        (0x00, 0),           // X  (lX @0x00)
        (0x04, 0),           // Y  (lY @0x04)
        (0x08, 0),           // Z  (lZ @0x08)
        (0x0C, 0),           // buttons (rgbButtons @0x0C)
    ];
    with_typed_write::<DiDataFormat, _, _>(engine, FORMAT_VA, |format| {
        format.size = 32;
        format.object_size = 4;
        format.flags = 0x0000_0001; // DIDF_CDATAFORMAT
        format.data_size = 16;
        format.num_objects = ENTRIES.len() as u32;
        format.objects = OBJECTS_VA;
        Ok(())
    })
    .expect("write the guest mouse DIDATAFORMAT");
    for (index, (offset, collection)) in ENTRIES.into_iter().enumerate() {
        let va = OBJECTS_VA + index as u64 * 24;
        with_typed_write::<DiObjectDataFormat, _, _>(engine, va, |object| {
            object.guid = 0;
            object.offset = offset;
            object.object_type = 0;
            object.flags = collection;
            Ok(())
        })
        .expect("write a guest DIOBJECTDATAFORMAT");
    }
}

/// A test engine whose guest heap can actually hand out memory.
///
/// `default_winapi_state` attaches a shared guest control block at `0x2000`, and
/// `alloc_coherent` reads its bump cursor from *guest memory* there. The
/// zeroed page that leaves the cursor at 0, which is below the heap base, so
/// every allocation fails — the same seeding the EnumDisplayMonitors test
/// documents. The real runtime seeds the same cursor at startup.
fn dinput_test_engine() -> IcedCpu {
    let mut engine = super::test_engine();
    engine
        .mem_write(0x2000, &0x3000_u64.to_le_bytes())
        .expect("seed guest heap bump cursor");
    engine
}

/// Press `vk` in the guest keyboard array the way the host seam does.
fn press(state: &mut WinApiState, vk: usize) {
    state.window_state().keyboard_state.set(vk, 0x80);
}

/// Run `DirectInput8Create` and return the object address it handed back.
fn create_direct_input(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    engine
        .mem_write(0x5000, &0_u64.to_le_bytes())
        .expect("seed the DirectInput8Create out-pointer");
    write_regs(engine, 0, 0x0800, 0x5000, 0, STACK_TOP);
    assert_return_value!(
        dispatch_winapi(
            &mut HandlerContext::new(engine, test_environment(), state),
            "dinput8.dll",
            "DirectInput8Create"
        ),
        0 // DI_OK
    );
    let mut raw = [0_u8; 8];
    engine
        .mem_read(0x5000, &mut raw)
        .expect("read the IDirectInput8 pointer");
    u64::from_le_bytes(raw)
}

/// Call one `IDirectInput8::Xxx` vtable stop through the real dispatch path,
/// so the test exercises name resolution and the handler together. `this` is
/// RCX, the way the guest passes it.
#[allow(clippy::too_many_arguments)]
fn call_object(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    this: u64,
    rdx: u64,
    r8: u64,
    r9: u64,
    method: &str,
) -> u64 {
    let name = format!("IDirectInput8::{method}");
    write_regs(engine, this, rdx, r8, r9, STACK_TOP);
    dispatch_winapi(
        &mut HandlerContext::new(engine, test_environment(), state),
        "dinput8.dll",
        &name,
    )
    .expect("IDirectInput8 method should dispatch")
    .return_value
}

/// Call one `IDirectInputDevice8::Xxx` vtable stop through the real dispatch
/// path. `this` is RCX, the way the guest passes it.
#[allow(clippy::too_many_arguments)]
fn call_device(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    this: u64,
    rdx: u64,
    r8: u64,
    r9: u64,
    method: &str,
) -> u64 {
    let name = format!("IDirectInputDevice8::{method}");
    write_regs(engine, this, rdx, r8, r9, STACK_TOP);
    dispatch_winapi(
        &mut HandlerContext::new(engine, test_environment(), state),
        "dinput8.dll",
        &name,
    )
    .expect("IDirectInputDevice8 method should dispatch")
    .return_value
}

/// Create a device of `class` and return its guest object address.
fn create_device(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    this: u64,
    class: DInputDeviceClass,
) -> u64 {
    engine
        .mem_write(0x6000, &class.instance_guid())
        .expect("write the device instance GUID");
    engine
        .mem_write(0x5100, &0_u64.to_le_bytes())
        .expect("seed the CreateDevice out-pointer");
    let hr = call_object(engine, state, this, 0x6000, 0x5100, 0, "CreateDevice");
    assert_eq!(hr, 0, "CreateDevice must succeed for {class:?}");
    let mut raw = [0_u8; 8];
    engine
        .mem_read(0x5100, &mut raw)
        .expect("read the device pointer");
    u64::from_le_bytes(raw)
}

// ── 1. The DLL is a WinAPI library and its export reaches a real handler ──

/// `dinput8.dll` must be classified as a WinAPI library (otherwise a guest's
/// static import is treated as a loadable guest module and the PE fails to
/// load) and `DirectInput8Create` must resolve to a real handler rather than
/// the "unsupported API" placeholder.
#[test]
fn dinput8_dll_is_a_winapi_library_with_a_real_handler() {
    for name in ["dinput8.dll", "DINPUT8.DLL", "Dinput8.Dll"] {
        assert!(
            is_winapi_library(name),
            "{name} must be a WinAPI library, not a guest module"
        );
    }
    assert!(
        is_winapi_implemented("dinput8.dll", "DirectInput8Create"),
        "DirectInput8Create must resolve to a real handler"
    );
    assert!(
        is_winapi_implemented("DINPUT8.DLL", "directinput8create"),
        "export lookup must be case-insensitive"
    );
    // A name dinput8.dll does not export must NOT claim to be implemented, or
    // the loader would hand a guest a soft placeholder for a missing stub.
    assert!(
        !is_winapi_implemented("dinput8.dll", "DllCanUnloadNow"),
        "an unexported name must not be reported as implemented"
    );

    // And the dispatch chain really reaches the handler.
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x5000, &0_u64.to_le_bytes())
        .expect("seed the out-pointer");
    write_regs(&mut engine, 0, 0x0800, 0x5000, 0, STACK_TOP);
    let result = dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "dinput8.dll",
        "DirectInput8Create",
    )
    .expect("dispatch must succeed");
    assert_eq!(result.return_value, 0, "DI_OK");
}

/// `DirectInput8Create` builds a real guest `IDirectInput8`: every one of the
/// 11 vtable slots (dinput.h:2402-2417) points at WIE's COM stop for that
/// (interface, slot) pair, and the object's first word points at the vtable.
#[test]
fn direct_input8_create_builds_the_guest_vtable() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    assert_ne!(object, 0, "DirectInput8Create must return an object");

    let mut vtable_raw = [0_u8; 8];
    engine
        .mem_read(object, &mut vtable_raw)
        .expect("read the object's vtable pointer");
    let vtable = u64::from_le_bytes(vtable_raw);

    for slot in 0..DirectInput8Method::VTABLE_SLOTS {
        let mut entry = [0_u8; 8];
        engine
            .mem_read(vtable + slot as u64 * 8, &mut entry)
            .expect("read a vtable entry");
        let expected = encode_com_dinput8(
            DInput8Iface::DirectInput8,
            u8::try_from(slot).expect("slot fits u8"),
        );
        assert_eq!(
            u64::from_le_bytes(entry),
            expected,
            "IDirectInput8 vtable slot {slot}"
        );
    }
    assert_eq!(
        state.dinput8().direct_input_object,
        object,
        "the created object must be the live one"
    );
    assert_eq!(state.dinput8().ref_count, 1, "a fresh object is refcount 1");
}

/// A `dwVersion` other than 0x0800 is refused with
/// `DIERR_OLDDIRECTINPUTVERSION` rather than silently negotiated — the
/// contract a DirectInput8 guest relies on to detect a missing DLL.
#[test]
fn direct_input8_create_rejects_a_non_0800_version() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x5000, &0_u64.to_le_bytes())
        .expect("seed the out-pointer");
    write_regs(&mut engine, 0, 0x0500, 0x5000, 0, STACK_TOP);
    assert_return_value!(
        dispatch_winapi(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            "dinput8.dll",
            "DirectInput8Create"
        ),
        0x8007_0081 // DIERR_OLDDIRECTINPUTVERSION
    );
    assert_eq!(
        state.dinput8().direct_input_object,
        0,
        "a refused create must not register an object"
    );
}

// ── 2. CreateDevice: the two real devices, and an honest failure ─────────

/// `CreateDevice` hands back a real 32-slot `IDirectInputDevice8` vtable
/// (dinput.h:1992-2031) for both devices WIE has.
#[test]
fn create_device_builds_the_device_vtable_for_both_devices() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    for class in [DInputDeviceClass::Keyboard, DInputDeviceClass::Mouse] {
        let device = create_device(&mut engine, &mut state, object, class);
        assert_ne!(device, 0, "{class:?} must produce a device object");
        let mut vtable_raw = [0_u8; 8];
        engine
            .mem_read(device, &mut vtable_raw)
            .expect("read the device object's vtable pointer");
        let vtable = u64::from_le_bytes(vtable_raw);
        for slot in 0..DirectInputDevice8Method::VTABLE_SLOTS {
            let mut entry = [0_u8; 8];
            engine
                .mem_read(vtable + slot as u64 * 8, &mut entry)
                .expect("read a device vtable entry");
            assert_eq!(
                u64::from_le_bytes(entry),
                encode_com_dinput8(
                    DInput8Iface::DirectInputDevice8,
                    u8::try_from(slot).expect("slot fits u8")
                ),
                "{class:?} device vtable slot {slot}"
            );
        }
        assert_eq!(
            state
                .dinput8()
                .device(device)
                .expect("device is registered")
                .class,
            class,
            "the record must remember which device this is"
        );
    }
}

/// A device class WIE does not have (here: a real machine's HID/product GUID,
/// standing in for any joystick, gamepad, flight control, or HID device) must
/// fail with `DIERR_INVALIDPARAM` and a NULLed out-pointer — never a device
/// object that silently does nothing.
#[test]
fn create_device_for_an_unsupported_class_fails_honestly() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    // A well-formed but unknown GUID (version-4 UUID shape, all zeros in the
    // body — nothing WIE synthesizes).
    engine
        .mem_write(0x6000, &[0x11_u8; 16])
        .expect("write an unknown device GUID");
    engine
        .mem_write(0x5100, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("seed the out-pointer with a canary");
    let hr = call_object(
        &mut engine,
        &mut state,
        object,
        0x6000,
        0x5100,
        0,
        "CreateDevice",
    );

    assert_eq!(
        hr, 0x8007_0057,
        "DIERR_INVALIDPARAM for a device class WIE does not have"
    );
    let mut raw = [0_u8; 8];
    engine
        .mem_read(0x5100, &mut raw)
        .expect("read the out-pointer");
    assert_eq!(
        u64::from_le_bytes(raw),
        0,
        "a failed CreateDevice must null the out-pointer, not leave the canary"
    );
    assert!(
        state.dinput8().devices.is_empty(),
        "a failed CreateDevice must not register a device"
    );
}

/// `GetDeviceStatus` is `DI_OK` for the two devices WIE has and
/// `DI_NOTATTACHED` (`S_FALSE`) for everything else — the honest "not plugged
/// in" a DirectInput guest already knows how to handle.
#[test]
fn get_device_status_is_ok_for_our_devices_and_s_false_otherwise() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    for class in [DInputDeviceClass::Keyboard, DInputDeviceClass::Mouse] {
        engine
            .mem_write(0x6000, &class.instance_guid())
            .expect("write the device GUID");
        let hr = call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0,
            0,
            "GetDeviceStatus",
        );
        assert_eq!(hr, 0, "DI_OK for {class:?}");
    }

    // An unknown GUID, and a NULL GUID, both report "not attached".
    engine
        .mem_write(0x6000, &[0x22_u8; 16])
        .expect("write an unknown GUID");
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0,
            0,
            "GetDeviceStatus"
        ),
        1, // DI_NOTATTACHED == S_FALSE
        "an unknown device is not attached"
    );
    assert_eq!(
        call_object(&mut engine, &mut state, object, 0, 0, 0, "GetDeviceStatus"),
        1,
        "a NULL GUID is not attached"
    );
}

// ── 3. GetDeviceState: the DIDEVICEOBJECTDATA round trip ─────────────────

/// The keyboard round trip: seed the live keyboard array the way the host seam
/// does, declare `c_dfDIKeyboard`'s object array, and assert the
/// `DIDEVICEOBJECTDATA` records WIE writes — at the raw mingw
/// `dinput.h:715-723` offsets, with the pressed key's value in the right slot.
#[test]
fn get_device_state_reports_pressed_keys_as_dideviceobjectdata() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Keyboard);

    write_keyboard_format(&mut engine, 256);
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            FORMAT_VA,
            0,
            0,
            "SetDataFormat"
        ),
        0,
        "SetDataFormat"
    );

    // VK_A (0x41) and VK_LSHIFT (0xA0) down, VK_Z up.
    press(&mut state, 0x41);
    press(&mut state, 0xA0);

    const REPORT_VA: u64 = 0xC000;
    let hr = call_device(
        &mut engine,
        &mut state,
        device,
        1024,
        REPORT_VA,
        0,
        "GetDeviceState",
    );
    assert_eq!(hr, 0, "DI_OK");

    // Read the raw bytes of the whole report: 256 records x 24 bytes.
    let mut report = vec![0_u8; 256 * 24];
    engine
        .mem_read(REPORT_VA, &mut report)
        .expect("read the DIDEVICEOBJECTDATA report");

    let u32_at = |bytes: &[u8], record: usize, field: usize| -> u32 {
        let base = record * 24 + field;
        let mut raw = [0_u8; 4];
        raw.copy_from_slice(&bytes[base..base + 4]);
        u32::from_le_bytes(raw)
    };
    let u64_at = |bytes: &[u8], record: usize, field: usize| -> u64 {
        let base = record * 24 + field;
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&bytes[base..base + 8]);
        u64::from_le_bytes(raw)
    };

    // Record N describes the object at `dwOfs = N * 4` — the guest's own
    // declared offset, echoed back so it can cross-check its array.
    for record in 0..256 {
        assert_eq!(
            u32_at(&report, record, 0x00),
            (record as u32) * 4,
            "DIDEVICEOBJECTDATA::dwOfs @0x00 for record {record}"
        );
    }
    // DIK/VK 0x41 ('A') and 0xA0 (left shift) report 0x80 ("down"); 0x5A ('Z')
    // reports 0.
    assert_eq!(u32_at(&report, 0x41, 0x04), 0x80, "dwData @0x04 for 'A'");
    assert_eq!(u32_at(&report, 0xA0, 0x04), 0x80, "dwData @0x04 for LSHIFT");
    assert_eq!(u32_at(&report, 0x5A, 0x04), 0, "an unpressed key reads 0");
    // Every record in one report shares the timestamp and the sequence number.
    let sequence = u32_at(&report, 0x41, 0x0C);
    for record in 0..256_u32 {
        assert_eq!(
            u32_at(&report, usize::try_from(record).unwrap_or(0), 0x0C),
            sequence,
            "dwSequence @0x0C is per-report, not per-object"
        );
        assert_eq!(
            u32_at(&report, usize::try_from(record).unwrap_or(0), 0x08),
            u32_at(&report, 0x41, 0x08),
            "dwTimeStamp @0x08 is per-report too"
        );
    }
    assert_eq!(
        u64_at(&report, 0x41, 0x10),
        0,
        "uAppData @0x10 is the DI8 tail and is zero"
    );
    // The DI8 DIDEVICEOBJECTDATA is 24 bytes, not the 16-byte DX3 shape: the
    // report is 256 records over 1024 bytes, and record 255's uAppData at
    // 255*24+0x10 = 0x17F0..0x17F8 must still be inside what was written.
    assert_eq!(
        u64_at(&report, 255, 0x10),
        0,
        "the 24-byte DI8 layout holds for the last record too"
    );

    // `dwSequence` is a monotonic per-device event counter, so it must ADVANCE
    // between successive reports — a guest uses it to spot a dropped or stale
    // state read. (Uniformity within one report is checked above.)
    call_device(
        &mut engine,
        &mut state,
        device,
        1024,
        REPORT_VA,
        0,
        "GetDeviceState",
    );
    engine
        .mem_read(REPORT_VA, &mut report)
        .expect("read the second DIDEVICEOBJECTDATA report");
    let next_sequence = u32_at(&report, 0x41, 0x0C);
    assert_eq!(
        next_sequence,
        sequence.wrapping_add(1),
        "dwSequence must advance by exactly one per report"
    );
}

/// `GetDeviceState` refuses honestly when it cannot report: an unknown device,
/// a device with no data format, and a buffer smaller than the format's
/// `dwDataSize` each get a distinct, documented `HRESULT` rather than a
/// partially-filled report.
#[test]
fn get_device_state_failure_modes_are_distinct_and_honest() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Keyboard);

    // No SetDataFormat yet.
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            0xBAD0,
            1024,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0x8007_0002,
        "an unknown device is DIERR_NOTFOUND"
    );
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            1024,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0x8007_0005,
        "a device with no data format is DIERR_NOTACQUIRED"
    );

    write_keyboard_format(&mut engine, 256);
    call_device(
        &mut engine,
        &mut state,
        device,
        FORMAT_VA,
        0,
        0,
        "SetDataFormat",
    );
    // dwDataSize is 256 * 4 = 1024; ask for less.
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            512,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0x8007_0057,
        "a short buffer is DIERR_INVALIDPARAM"
    );
}

/// The mouse round trip: the report is the standard `DIMOUSESTATE` shape, and
/// the X/Y axes are *relative* — differenced against the previous report and
/// consumed, which is what a DirectInput relative axis means.
#[test]
fn get_device_state_mouse_axes_are_relative_and_consumed() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);
    write_mouse_format(&mut engine);
    call_device(
        &mut engine,
        &mut state,
        device,
        FORMAT_VA,
        0,
        0,
        "SetDataFormat",
    );

    const REPORT_VA: u64 = 0xC000;
    let read_axes = |engine: &mut IcedCpu| -> (i32, i32) {
        let mut report = vec![0_u8; 5 * 24];
        engine
            .mem_read(REPORT_VA, &mut report)
            .expect("read the mouse report");
        // Record 1 is lX @0x00, record 2 is lY @0x04 (the guest's declared
        // offsets, echoed into dwOfs).
        let read = |record: usize, field: usize| -> i32 {
            let base = record * 24 + field;
            let mut raw = [0_u8; 4];
            raw.copy_from_slice(&report[base..base + 4]);
            i32::from_le_bytes(raw)
        };
        assert_eq!(read(1, 0x00), 0, "record 1's dwOfs is lX's offset 0x00");
        assert_eq!(read(2, 0x00), 4, "record 2's dwOfs is lY's offset 0x04");
        assert_eq!(read(3, 0x00), 8, "record 3's dwOfs is lZ's offset 0x08");
        assert_eq!(read(4, 0x00), 12, "record 4's dwOfs is rgbButtons' 0x0C");
        (read(1, 0x04), read(2, 0x04))
    };

    // The host publishes an absolute cursor position; the first report has no
    // previous position, so the honest delta is zero.
    state.present().channel.push_cursor_pos(100, 50);
    call_device(
        &mut engine,
        &mut state,
        device,
        16,
        REPORT_VA,
        0,
        "GetDeviceState",
    );
    assert_eq!(
        read_axes(&mut engine),
        (0, 0),
        "the first report has no previous position to difference against"
    );

    // Move the cursor by (+7, -3) and read again: the axes report the delta.
    state.present().channel.push_cursor_pos(107, 47);
    call_device(
        &mut engine,
        &mut state,
        device,
        16,
        REPORT_VA,
        0,
        "GetDeviceState",
    );
    assert_eq!(
        read_axes(&mut engine),
        (7, -3),
        "lX/lY are the cursor delta"
    );

    // The delta is consumed: reading again with the cursor unchanged reports 0.
    call_device(
        &mut engine,
        &mut state,
        device,
        16,
        REPORT_VA,
        0,
        "GetDeviceState",
    );
    assert_eq!(
        read_axes(&mut engine),
        (0, 0),
        "a relative axis is consumed on read"
    );
}

// ── 4. EnumDevices visits BOTH devices ───────────────────────────────────

/// The regression guard for the enumeration-continuation wiring: a guest's
/// `DIDEVICEENUMCALLBACK` must be invoked **once per device** — keyboard then
/// mouse — and the walk must then complete. A build that only ever delivered
/// the first device (the failure mode the `gdi32::enumerate` router exists to
/// prevent) would visit one and stop here.
#[test]
fn enum_devices_visits_the_keyboard_and_then_the_mouse() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    // RCX=this, RDX=dwDevType(0 = all), R8=callback, R9=pvRef, [rsp+0x28]=flags.
    const CALLBACK: u64 = 0x9000;
    const PVREF: u64 = 0x1234;
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write EnumDevices dwFlags");
    write_regs(&mut engine, object, 0, CALLBACK, PVREF, STACK_TOP);

    // Visit 1: the first callback, with the keyboard instance record.
    let error = dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "dinput8.dll",
        "IDirectInput8::EnumDevices",
    )
    .expect_err("EnumDevices must bridge to the guest callback");
    let (instance_va, enumeration_id) = enumeration_signal(&error);

    // The callback's RCX is the instance record, RDX is pvRef (full 64 bits).
    assert_ne!(instance_va, 0, "RCX is the DIDEVICEINSTANCEA");
    assert_eq!(device_type_at(&mut engine, instance_va), 0x1303, "keyboard");

    // Continue (DIENUM_CONTINUE) -> visit 2 must be the MOUSE. Driven through
    // `gdi32::enumerate::advance_enumeration` — the entry point the runtime
    // actually calls — so this test also proves the router offers the id to the
    // DirectInput lane before falling through to the font path.
    let next =
        crate::gdi32::enumerate::advance_enumeration(&mut engine, &mut state, enumeration_id)
            .expect("advance the enumeration")
            .expect("a second device must remain: the mouse");
    assert_eq!(next.callback_address, CALLBACK, "same callback");
    assert_eq!(next.long_parameter, PVREF, "pvRef forwarded verbatim");
    assert_eq!(
        device_type_at(&mut engine, instance_va),
        0x1202,
        "the second visit is the mouse, in the same record buffer"
    );
    // The request re-enters with the instance record in RCX.
    assert_eq!(next.window_handle, instance_va, "RCX is the record again");
    // `EnumDevices` returns DI_OK when the walk completes, never the
    // callback's BOOL.
    assert_eq!(
        next.outer_return,
        OuterReturn::Fixed(0),
        "outer return DI_OK"
    );

    // Continue again -> the walk is complete and the state is dropped.
    let done =
        crate::gdi32::enumerate::advance_enumeration(&mut engine, &mut state, enumeration_id)
            .expect("advance past the last device");
    assert!(
        done.is_none(),
        "the enumeration must complete after the mouse"
    );
    // A fourth advance is a no-op (the id is gone), not a panic or a rewind.
    let after =
        crate::gdi32::enumerate::advance_enumeration(&mut engine, &mut state, enumeration_id)
            .expect("advance a completed enumeration");
    assert!(after.is_none(), "a completed enumeration stays completed");
    // The font lane is untouched by all of this: an id nobody owns still
    // resolves to "complete" through the same router.
    let unknown = crate::gdi32::enumerate::advance_enumeration(&mut engine, &mut state, u64::MAX)
        .expect("an unowned id must be a clean no-op");
    assert!(unknown.is_none(), "an unowned enumeration id completes");
}

/// A `dwDevType` filter naming a device class WIE does not have must complete
/// **without ever calling back** — a guest probing for a joystick is told
/// there is none, not handed silence followed by a success it could read as
/// "a joystick exists".
#[test]
fn enum_devices_for_an_absent_class_never_calls_back() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    // DIDEVTYPE_JOYSTICK (dinput.h:228).
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write EnumDevices dwFlags");
    write_regs(&mut engine, object, 4, 0x9000, 0, STACK_TOP);
    let result = dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "dinput8.dll",
        "IDirectInput8::EnumDevices",
    )
    .expect("dispatch must succeed");
    assert_eq!(result.return_value, 0, "DI_OK, with nothing to report");
}

/// The `DI8DEVTYPE_*` classes WIE has no device for are a checked constant, and
/// `EnumDevices` really does filter them out (a guest probing for a joystick
/// gets "none", not a success it could read as "one exists"). Also pins the
/// `DI8DEVCLASS_*` mapping and the `dwDevType` values, which are what a guest's
/// own device-class check reads out of `DIDEVICEINSTANCE`.
#[test]
fn the_absent_device_classes_are_filtered_out_and_the_present_ones_are_not() {
    // dinput.h:240-241 — DI8DEVTYPE_JOYSTICK / DI8DEVTYPE_GAMEPAD.
    assert_eq!(
        crate::dinput::UNSUPPORTED_DEVTYPES,
        [0x14, 0x15],
        "the absent DI8DEVTYPE_* classes"
    );
    for absent in crate::dinput::UNSUPPORTED_DEVTYPES {
        for class in [DInputDeviceClass::Keyboard, DInputDeviceClass::Mouse] {
            assert!(
                !class.matches_dev_type(u64::from(absent)),
                "{class:?} must not match the absent class {absent:#x}"
            );
        }
    }

    // (DI8DEVTYPE_KEYBOARD << 8) | DIDEVTYPE_KEYBOARD, and the mouse spelling.
    assert_eq!(
        DInputDeviceClass::Keyboard.dev_type(),
        0x1303,
        "keyboard dwDevType"
    );
    assert_eq!(
        DInputDeviceClass::Mouse.dev_type(),
        0x1202,
        "mouse dwDevType"
    );
    assert_eq!(
        DInputDeviceClass::Keyboard.dev_class(),
        3,
        "DI8DEVCLASS_KEYBOARD (dinput.h:234)"
    );
    assert_eq!(
        DInputDeviceClass::Mouse.dev_class(),
        2,
        "DI8DEVCLASS_POINTER (dinput.h:233)"
    );

    // 0 means "all devices"; a bare DIDEVTYPE_*, a DI8DEVTYPE_*, and a full
    // dwDevType all select.
    assert!(
        DInputDeviceClass::Mouse.matches_dev_type(0),
        "0 = all devices"
    );
    assert!(
        DInputDeviceClass::Mouse.matches_dev_type(2),
        "DIDEVTYPE_MOUSE (dinput.h:226)"
    );
    assert!(
        DInputDeviceClass::Mouse.matches_dev_type(0x12),
        "DI8DEVTYPE_MOUSE (dinput.h:238)"
    );
    assert!(
        DInputDeviceClass::Mouse.matches_dev_type(0x1202),
        "a full dwDevType"
    );
    assert!(
        !DInputDeviceClass::Mouse.matches_dev_type(3),
        "DIDEVTYPE_KEYBOARD must not select the mouse"
    );
    assert!(
        !DInputDeviceClass::Keyboard.matches_dev_type(2),
        "DIDEVTYPE_MOUSE must not select the keyboard"
    );
}

/// `dinput8.dll` exports `DirectInput8Create` by ORDINAL 1 only, so a
/// late-bound guest reaches it through `GetProcAddress`. That route must land
/// on a real fake VA (not NULL, and not a "soft placeholder" the loader would
/// report as an unsupported stub).
#[test]
fn direct_input8_create_is_reachable_through_get_proc_address() {
    let va = crate::dynamic_apis::dynamic_fake_target_va("dinput8.dll", "DirectInput8Create")
        .expect("GetProcAddress must resolve DirectInput8Create");
    assert_ne!(va, 0, "the resolved fake VA must not be NULL");
    assert_eq!(
        crate::dynamic_apis::dynamic_fake_target_va("dinput8.dll", "directinput8create"),
        Some(va),
        "GetProcAddress lookup is case-insensitive"
    );
    assert_eq!(
        crate::dynamic_apis::dynamic_fake_target_va("DINPUT8.DLL", "DIRECTINPUT8CREATE"),
        Some(va),
        "the library name is matched case-insensitively too"
    );
    // And the name-only `GetProcAddress` entry point, which is what a guest
    // that only has the module handle actually calls.
    assert_eq!(
        crate::resolve_get_proc_address("DirectInput8Create"),
        Some(va),
        "resolve_get_proc_address must reach the same fake VA"
    );
}

/// The `DIDEVICEENUMCALLBACK` return contract: a guest returns
/// `DIENUM_CONTINUE` (1) to keep the walk going and `DIENUM_STOP` (0) to stop
/// it (dinput.h:215-216). These are the exact values the runtime's
/// continuation test (`return value != 0`) is written against, so pinning them
/// here catches a "harmless" change that would break every DirectInput guest.
#[test]
fn enum_devices_return_contract_matches_the_header() {
    assert_eq!(
        crate::dinput::DIENUM_CONTINUE,
        1,
        "DIENUM_CONTINUE (dinput.h:216)"
    );
    assert_eq!(crate::dinput::DIENUM_STOP, 0, "DIENUM_STOP (dinput.h:215)");
}

/// Pull `(lpddi, enumeration_id)` out of the enumeration control signal.
fn enumeration_signal(error: &anyhow::Error) -> (u64, u64) {
    match error.downcast_ref::<WinApiControlSignal>() {
        Some(WinApiControlSignal::EnumerationCallbackRequested {
            request,
            enumeration_id,
        }) => (request.window_handle, *enumeration_id),
        other => panic!("expected an enumeration callback, got {other:?}"),
    }
}

/// `DIDEVICEINSTANCEA::dwDevType` at its mingw `dinput.h:443-455` offset.
fn device_type_at(engine: &mut IcedCpu, va: u64) -> u32 {
    let mut raw = [0_u8; 4];
    engine
        .mem_read(va + 0x24, &mut raw)
        .expect("read DIDEVICEINSTANCE::dwDevType");
    u32::from_le_bytes(raw)
}

// ── 5. The honest-failure surface ────────────────────────────────────────

/// Everything WIE has no implementation for must say so with a specific
/// `HRESULT` — never succeed silently, and never claim a device it does not
/// have. This is the "a guest probing for them gets an honest failure"
/// contract, pinned so a future change that starts faking success is caught.
#[test]
fn unsupported_directinput_methods_fail_honestly() {
    const DIERR_UNSUPPORTED: u64 = 0x8000_4001; // E_NOTIMPL
    const DI_NOTATTACHED: u64 = 1; // S_FALSE
    const DIERR_NOTFOUND: u64 = 0x8007_0002;

    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);

    // No force feedback.
    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "CreateEffect"),
        DIERR_UNSUPPORTED,
        "CreateEffect: E_NOTIMPL"
    );
    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "EnumEffects"),
        DI_NOTATTACHED,
        "EnumEffects: S_FALSE (no effects)"
    );
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            0,
            0,
            0,
            "GetForceFeedbackState"
        ),
        DIERR_UNSUPPORTED,
        "GetForceFeedbackState: E_NOTIMPL"
    );
    // No buffered event ring: this lane is polled state only.
    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "GetDeviceData"),
        DIERR_UNSUPPORTED,
        "GetDeviceData: E_NOTIMPL"
    );
    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "SendDeviceData"),
        DIERR_UNSUPPORTED,
        "SendDeviceData: E_NOTIMPL"
    );
    // No action maps, so nothing can be enumerated by semantics.
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0,
            0,
            0,
            "EnumDevicesBySemantics"
        ),
        DI_NOTATTACHED,
        "EnumDevicesBySemantics: S_FALSE (no action-mapped devices)"
    );
    // No device-configuration wizard.
    assert_eq!(
        call_object(&mut engine, &mut state, object, 0, 0, 0, "ConfigureDevices"),
        DIERR_UNSUPPORTED,
        "ConfigureDevices: E_NOTIMPL"
    );
    // `GetObjectInfo` names an object that does not exist in our positional
    // formats, so it is "not found", not "no data".
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            0xC000,
            0,
            0,
            "GetObjectInfo"
        ),
        DIERR_NOTFOUND,
        "GetObjectInfo: DIERR_NOTFOUND"
    );
    // `FindDevice` for a GUID WIE does not synthesize.
    engine
        .mem_write(0x6000, &[0x33_u8; 16])
        .expect("write an unknown product GUID");
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0,
            0x7000,
            "FindDevice"
        ),
        DIERR_NOTFOUND,
        "FindDevice: DIERR_NOTFOUND for an unknown product GUID"
    );
}

/// The methods WIE really does serve, on top of the COM wiring: a polled
/// device's `Poll` succeeds, cooperative level and data format are accepted and
/// stored, and the device info record reports the right identity.
#[test]
fn served_directinput_methods_succeed() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Keyboard);

    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "Poll"),
        0,
        "Poll: DI_OK for a known device"
    );
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            0x1234,
            0x0000_0002 | 0x0000_0004,
            0,
            "SetCooperativeLevel"
        ),
        0,
        "SetCooperativeLevel is accepted"
    );
    let record = state
        .dinput8()
        .device(device)
        .expect("device is registered");
    assert_eq!(record.cooperative_hwnd, 0x1234, "the HWND is stored");
    assert_eq!(
        record.cooperative_flags, 0x0000_0006,
        "the DISCL_* flags are stored (not enforced)"
    );

    // `GetDeviceInfo` fills the identity record the guest asked to be sized.
    engine
        .mem_write(0xC000, &580_u32.to_le_bytes())
        .expect("size the DIDEVICEINSTANCEA");
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            0xC000,
            0,
            0,
            "GetDeviceInfo"
        ),
        0,
        "GetDeviceInfo: DI_OK"
    );
    assert_eq!(
        device_type_at(&mut engine, 0xC000),
        0x1303,
        "keyboard dwDevType"
    );
    let mut instance = vec![0_u8; 580];
    engine
        .mem_read(0xC000, &mut instance)
        .expect("read the instance record");
    // `tszInstanceName` @0x28 and `tszProductName` @0x12C, both
    // `CHAR[MAX_PATH]`; the name plus its NUL must land byte-for-byte.
    const KEYBOARD_NAME: &[u8] = b"WIE Virtual Keyboard\0";
    assert_eq!(
        &instance[0x28..0x28 + KEYBOARD_NAME.len()],
        KEYBOARD_NAME,
        "tszInstanceName @0x28"
    );
    assert_eq!(
        &instance[0x12C..0x12C + KEYBOARD_NAME.len()],
        KEYBOARD_NAME,
        "tszProductName @0x12C"
    );
    assert!(
        instance[0x28 + KEYBOARD_NAME.len()..0x12C]
            .iter()
            .all(|byte| *byte == 0),
        "the rest of tszInstanceName stays NUL-padded"
    );
    assert_eq!(
        &instance[0x230..0x240],
        [0_u8; 16],
        "guidFFDriver @0x230 is zeroed"
    );
    // guidProduct must be the value CreateDevice accepts.
    engine
        .mem_write(0x6000, &DInputDeviceClass::Keyboard.product_guid())
        .expect("write the product GUID");
    engine
        .mem_write(0x5100, &0_u64.to_le_bytes())
        .expect("seed the out-pointer");
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0x5100,
            0,
            "CreateDevice"
        ),
        0,
        "the reported guidProduct must be usable with CreateDevice"
    );
}

/// `GetCapabilities` reports the device's real shape and echoes the guest's own
/// `dwSize`; a `dwSize` too small to hold the DX3 24-byte floor is refused
/// rather than half-written.
#[test]
fn get_capabilities_reports_the_real_shape() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    for (class, axes, buttons) in [
        (DInputDeviceClass::Keyboard, 0_u32, 256_u32),
        // 3, not 2: X, Y and the wheel as Z — see
        // `get_capabilities_counts_the_mouse_wheel_as_a_third_axis`.
        (DInputDeviceClass::Mouse, 3, 4),
    ] {
        let device = create_device(&mut engine, &mut state, object, class);
        engine
            .mem_write(0xC000, &44_u32.to_le_bytes())
            .expect("size the DIDEVCAPS");
        assert_eq!(
            call_device(
                &mut engine,
                &mut state,
                device,
                0xC000,
                0,
                0,
                "GetCapabilities"
            ),
            0,
            "GetCapabilities: DI_OK"
        );
        let mut caps = vec![0_u8; 44];
        engine
            .mem_read(0xC000, &mut caps)
            .expect("read the DIDEVCAPS");
        let u32_at = |offset: usize| -> u32 {
            let mut raw = [0_u8; 4];
            raw.copy_from_slice(&caps[offset..offset + 4]);
            u32::from_le_bytes(raw)
        };
        assert_eq!(u32_at(0x00), 44, "dwSize @0x00 is echoed back");
        assert_eq!(u32_at(0x08), class.dev_type(), "dwDevType @0x08");
        assert_eq!(u32_at(0x0C), axes, "dwAxes @0x0C");
        assert_eq!(u32_at(0x10), buttons, "dwButtons @0x10");
        assert_eq!(u32_at(0x14), 0, "dwPOVs @0x14 — WIE has no hats");
        assert_eq!(u32_at(0x18), 0, "dwFFSamplePeriod @0x18 — no FF");
        assert_eq!(u32_at(0x28), 0, "dwFFDriverVersion @0x28 — no FF");
    }

    // A `dwSize` below the DX3 24-byte floor cannot hold `dwPOVs`.
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);
    engine
        .mem_write(0xC000, &16_u32.to_le_bytes())
        .expect("size the DIDEVCAPS too small");
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            0xC000,
            0,
            0,
            "GetCapabilities"
        ),
        0x8007_0057,
        "a too-small DIDEVCAPS is DIERR_INVALIDPARAM"
    );
}

/// `DIDEVCAPS::dwAxes` counts the mouse wheel: 3 axes (`lX`, `lY`, and the wheel
/// as `lZ`), which is what a real DirectInput wheel mouse reports and what
/// WIE's own `c_dfDIMouse` declares. The number used to be 2 while `lZ` carried
/// real wheel data, so a guest that sized its own report from `dwAxes`
/// truncated the axis it was about to read.
#[test]
fn get_capabilities_counts_the_mouse_wheel_as_a_third_axis() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    let read_axes = |engine: &mut IcedCpu, state: &mut WinApiState, class| -> u32 {
        let device = create_device(engine, state, object, class);
        engine
            .mem_write(0xC000, &44_u32.to_le_bytes())
            .expect("size the DIDEVCAPS");
        assert_eq!(
            call_device(engine, state, device, 0xC000, 0, 0, "GetCapabilities"),
            0,
            "GetCapabilities: DI_OK"
        );
        let mut caps = [0_u8; 44];
        engine
            .mem_read(0xC000, &mut caps)
            .expect("read the DIDEVCAPS");
        let mut dw_axes = [0_u8; 4];
        dw_axes.copy_from_slice(&caps[0x0C..0x10]);
        u32::from_le_bytes(dw_axes)
    };

    assert_eq!(
        read_axes(&mut engine, &mut state, DInputDeviceClass::Mouse),
        3,
        "dwAxes @0x0C must count lX, lY and the wheel's lZ"
    );
    assert_eq!(
        read_axes(&mut engine, &mut state, DInputDeviceClass::Keyboard),
        0,
        "a keyboard still has no analog axes"
    );
}

/// `Acquire` / `Unacquire` are recorded but not enforced, and `GetDeviceState`
/// therefore never returns `DIERR_NOTACQUIRED` for a formatted device — a guest
/// that forgets to acquire still gets real data instead of a silent zero.
#[test]
fn acquire_is_recorded_and_never_blocks_a_state_read() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Keyboard);
    write_keyboard_format(&mut engine, 256);
    call_device(
        &mut engine,
        &mut state,
        device,
        FORMAT_VA,
        0,
        0,
        "SetDataFormat",
    );

    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "Acquire"),
        0,
        "Acquire"
    );
    assert!(state.dinput8().device(device).expect("device").acquired);
    assert_eq!(
        call_device(&mut engine, &mut state, device, 0, 0, 0, "Unacquire"),
        0,
        "Unacquire"
    );
    assert!(!state.dinput8().device(device).expect("device").acquired);

    // Reading without acquiring still works: WIE has one global keyboard.
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            1024,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0,
        "GetDeviceState must not require Acquire"
    );
}

/// `IUnknown::QueryInterface` answers only `IID_IUnknown`; anything else gets
/// a NULLed out-pointer and `DIERR_NOINTERFACE` — the honest answer, since WIE
/// hands out these two interfaces and no others.
#[test]
fn query_interface_answers_only_iid_iunknown() {
    const IID_IUNKNOWN: [u8; 16] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x46,
    ];
    const IID_SOMETHING_ELSE: [u8; 16] = [0xAB; 16];

    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);

    engine
        .mem_write(0x6000, &IID_IUNKNOWN)
        .expect("write IID_IUnknown");
    engine
        .mem_write(0x5100, &0_u64.to_le_bytes())
        .expect("seed the out-pointer");
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0x5100,
            0,
            "QueryInterface"
        ),
        0,
        "IID_IUnknown: DI_OK"
    );
    let mut raw = [0_u8; 8];
    engine
        .mem_read(0x5100, &mut raw)
        .expect("read the out-pointer");
    assert_eq!(u64::from_le_bytes(raw), object, "IID_IUnknown aliases self");

    engine
        .mem_write(0x6000, &IID_SOMETHING_ELSE)
        .expect("write an unknown IID");
    engine
        .mem_write(0x5100, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("seed the out-pointer with a canary");
    assert_eq!(
        call_object(
            &mut engine,
            &mut state,
            object,
            0x6000,
            0x5100,
            0,
            "QueryInterface"
        ),
        0x8000_4002, // E_NOINTERFACE
        "an unknown IID is E_NOINTERFACE"
    );
    engine
        .mem_read(0x5100, &mut raw)
        .expect("read the out-pointer");
    assert_eq!(
        u64::from_le_bytes(raw),
        0,
        "a failed QI must null the out-pointer"
    );
}

/// The wheel is the MOUSE device's state: polling an unrelated device (the
/// keyboard here) must not consume the movement the mouse report has not read
/// yet, or a guest that polls both devices in a loop would lose every notch.
#[test]
fn get_device_state_keyboard_does_not_consume_the_mouse_wheel() {
    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let keyboard = create_device(&mut engine, &mut state, object, DInputDeviceClass::Keyboard);
    let mouse = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);
    write_keyboard_format(&mut engine, 256);
    call_device(
        &mut engine,
        &mut state,
        keyboard,
        FORMAT_VA,
        0,
        0,
        "SetDataFormat",
    );
    write_mouse_format(&mut engine);
    call_device(
        &mut engine,
        &mut state,
        mouse,
        FORMAT_VA,
        0,
        0,
        "SetDataFormat",
    );

    state.present().channel.push_wheel_notches(false, 1);
    // The keyboard reports its own state...
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            keyboard,
            1024,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0,
        "DI_OK for the keyboard"
    );
    // ...and the wheel is still waiting for the mouse report that owns it.
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            mouse,
            16,
            0xC000,
            0,
            "GetDeviceState"
        ),
        0,
        "DI_OK for the mouse"
    );
    assert_eq!(
        read_mouse_z_and_buttons(&mut engine).0,
        120,
        "a keyboard report must not consume the mouse wheel"
    );
}

/// One host button mask, two APIs: a press pushed through the presenter mirror
/// must read *down* through DirectInput's `GetDeviceState` and through
/// `GetKeyState` for all five mouse buttons, and up again on release.
///
/// This is the assertion that the two spellings of "which button is down" are
/// one source of truth — the presenter mirror's `MK_*` mask — rather than a
/// DirectInput button table plus a keyboard array that nobody writes. It used to
/// pass only half: DirectInput saw the press, `GetKeyState` reported up.
#[test]
fn get_device_state_and_get_key_state_agree_on_every_mouse_button() {
    // (rgbButtons slot, VK code, MK bit) for all five mouse buttons. The slot
    // order and the MK bits come from WIE's one table (`MOUSE_BUTTON_SLOTS`),
    // restated here so the test asserts the guest-visible agreement rather than
    // the table itself.
    const BUTTONS: [(usize, u64, u16); 5] = [
        (0, 0x01, 0x0001), // VK_LBUTTON / MK_LBUTTON
        (1, 0x02, 0x0002), // VK_RBUTTON / MK_RBUTTON
        (2, 0x04, 0x0010), // VK_MBUTTON / MK_MBUTTON
        (3, 0x05, 0x0020), // VK_XBUTTON1 / MK_XBUTTON1
        (4, 0x06, 0x0040), // VK_XBUTTON2 / MK_XBUTTON2
    ];

    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);
    write_mouse_format(&mut engine);
    declare_buttons2_upper_half(&mut engine);
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            FORMAT_VA,
            0,
            0,
            "SetDataFormat"
        ),
        0,
        "SetDataFormat"
    );

    // One host push, two reads. `GetDeviceState` through the COM dispatch chain,
    // `GetKeyState` through the USER32 handler — the two APIs a guest mixes.
    let read_both = |engine: &mut IcedCpu, state: &mut WinApiState, vk: u64| -> ([u8; 8], u64) {
        // cbData must cover the whole declared `dwDataSize` (six objects of
        // 4 bytes), which is what a guest passing a `DIMOUSESTATE2`-sized
        // buffer does.
        assert_eq!(
            call_device(engine, state, device, 6 * 4, 0xC000, 0, "GetDeviceState"),
            0,
            "DI_OK"
        );
        let dinput_buttons = read_mouse_buttons2(engine);
        write_regs(engine, vk, 0, 0, 0, STACK_TOP);
        let key_state = user32::handle_get_key_state(&mut HandlerContext::new(
            engine,
            test_environment(),
            state,
        ))
        .expect("GetKeyState should dispatch")
        .return_value;
        (dinput_buttons, key_state)
    };

    // Nothing pressed: both APIs say up.
    let (dinput_buttons, key_state) = read_both(&mut engine, &mut state, 0x01);
    assert_eq!(dinput_buttons, [0_u8; 8], "an untouched mouse is all-up");
    assert_eq!(key_state, 0, "VK_LBUTTON agrees: up");

    // Each button in turn, alone: DirectInput's own slot and the VK the guest
    // would poll must flip together.
    for (slot, vk, mk) in BUTTONS {
        state.present().channel.push_mouse_buttons(mk);
        let (dinput_buttons, key_state) = read_both(&mut engine, &mut state, vk);
        assert_eq!(
            dinput_buttons.get(slot),
            Some(&0x80),
            "DirectInput rgbButtons[{slot}] reads down for MK {mk:#06x}"
        );
        assert_eq!(
            key_state, 0x8000,
            "GetKeyState(VK {vk:#04x}) reads down for the same MK {mk:#06x}"
        );
    }

    // All five at once, then all released: still one answer, not two.
    state
        .present()
        .channel
        .push_mouse_buttons(0x0001 | 0x0002 | 0x0010 | 0x0020 | 0x0040);
    let (dinput_buttons, key_state) = read_both(&mut engine, &mut state, 0x01);
    assert_eq!(
        &dinput_buttons[..5],
        &[0x80, 0x80, 0x80, 0x80, 0x80],
        "DirectInput reports all five down"
    );
    assert_eq!(key_state, 0x8000, "VK_LBUTTON reports down with the rest");

    state.present().channel.push_mouse_buttons(0);
    let (dinput_buttons, key_state) = read_both(&mut engine, &mut state, 0x01);
    assert_eq!(dinput_buttons, [0_u8; 8], "release reaches DirectInput");
    assert_eq!(key_state, 0, "release reaches GetKeyState too");
}

/// The mouse button and wheel bytes of one `DIDF_CDATAFORMAT` mouse report,
/// read straight out of guest memory at the record offsets `write_mouse_format`
/// declared: `lZ` is record 3 (`dwOfs` 0x08) and `rgbButtons` is record 4
/// (`dwOfs` 0x0C, whose `dwData` low word is the guest-visible byte array).
fn read_mouse_z_and_buttons(engine: &mut IcedCpu) -> (i32, [u8; 8]) {
    const REPORT_VA: u64 = 0xC000;
    let mut report = vec![0_u8; 5 * 24];
    engine
        .mem_read(REPORT_VA, &mut report)
        .expect("read the mouse report");
    let data_at = |record: usize| -> u32 {
        let base = record * 24 + 0x04;
        let mut raw = [0_u8; 4];
        raw.copy_from_slice(&report[base..base + 4]);
        u32::from_le_bytes(raw)
    };
    let z = i32::from_le_bytes(data_at(3).to_le_bytes());
    let button_word = data_at(4).to_le_bytes();
    let mut buttons = [0_u8; 8];
    for (slot, byte) in buttons.iter_mut().zip(button_word) {
        *slot = byte;
    }
    (z, buttons)
}

/// The eight `DIMOUSESTATE2::rgbButtons` bytes of one mouse report, read the
/// way a guest reads them: one `DIDEVICEOBJECTDATA` DWORD per declared object
/// (`dwData` at record offset 0x04), with a second button object at
/// `dwOfs` 0x10 for the `DIMOUSESTATE2` upper half.
///
/// `read_mouse_z_and_buttons` only declares the 4-byte `DIMOUSESTATE` button
/// field, so it cannot see the fifth button; a guest that wants `rgbButtons`
/// in full declares the extra object itself, which is what
/// `declare_buttons2_upper_half` does here.
fn read_mouse_buttons2(engine: &mut IcedCpu) -> [u8; 8] {
    const REPORT_VA: u64 = 0xC000;
    let mut report = vec![0_u8; 6 * 24];
    engine
        .mem_read(REPORT_VA, &mut report)
        .expect("read the mouse report");
    let dw_data = |record: usize| -> [u8; 4] {
        let base = record * 24 + 0x04;
        let mut raw = [0_u8; 4];
        raw.copy_from_slice(&report[base..base + 4]);
        raw
    };
    let mut buttons = [0_u8; 8];
    for (slot, byte) in buttons[..4].iter_mut().zip(dw_data(4)) {
        *slot = byte;
    }
    for (slot, byte) in buttons[4..].iter_mut().zip(dw_data(5)) {
        *slot = byte;
    }
    buttons
}

/// Add a sixth object to the mouse format: the `DIMOUSESTATE2` upper half of
/// `rgbButtons` (`dwOfs` 0x10), so a report can be asked for the fifth button.
///
/// `with_typed_write` zero-fills before handing over the struct, so every
/// `DIDATAFORMAT` field has to be restated here, not just the two that change.
fn declare_buttons2_upper_half(engine: &mut IcedCpu) {
    with_typed_write::<DiDataFormat, _, _>(engine, FORMAT_VA, |format| {
        format.size = 32;
        format.object_size = 4;
        format.flags = 0x0000_0001; // DIDF_CDATAFORMAT
        format.data_size = 6 * 4;
        format.num_objects = 6;
        format.objects = OBJECTS_VA;
        Ok(())
    })
    .expect("widen the guest mouse DIDATAFORMAT");
    let va = OBJECTS_VA + 5 * 24;
    with_typed_write::<DiObjectDataFormat, _, _>(engine, va, |object| {
        object.guid = 0;
        object.offset = 0x10;
        object.object_type = 0;
        object.flags = 0;
        Ok(())
    })
    .expect("declare the DIMOUSESTATE2 rgbButtons upper half");
}

/// A host mouse press/release and wheel movement must reach the mouse
/// `GetDeviceState` report: `rgbButtons[0..4]` carry the `MK_*` mask the host
/// pushed and `lZ` carries the wheel notches. Driven through the real dispatch
/// chain, and — the contract that used to be documented as a gap — the state
/// comes from the presenter-side window mirror, the same lock-free seam
/// `set_key_state` uses, not from a second button table.
#[test]
fn get_device_state_mouse_reports_host_buttons_and_wheel() {
    // Win32 `MK_*` bits (winuser.h).
    const MK_LBUTTON: u16 = 0x0001;
    const MK_MBUTTON: u16 = 0x0010;
    const WHEEL_DELTA: i32 = 120;

    let mut engine = dinput_test_engine();
    let mut state = default_winapi_state();
    let object = create_direct_input(&mut engine, &mut state);
    let device = create_device(&mut engine, &mut state, object, DInputDeviceClass::Mouse);
    write_mouse_format(&mut engine);
    assert_eq!(
        call_device(
            &mut engine,
            &mut state,
            device,
            FORMAT_VA,
            0,
            0,
            "SetDataFormat"
        ),
        0,
        "SetDataFormat"
    );

    // Nothing pressed, nothing scrolled: the honest all-up report.
    let read = |engine: &mut IcedCpu, state: &mut WinApiState| -> (i32, [u8; 8]) {
        assert_eq!(
            call_device(engine, state, device, 16, 0xC000, 0, "GetDeviceState"),
            0,
            "DI_OK"
        );
        read_mouse_z_and_buttons(engine)
    };

    let (z, buttons) = read(&mut engine, &mut state);
    assert_eq!(z, 0, "an untouched wheel reads 0");
    assert_eq!(buttons, [0_u8; 8], "no button reads down before a press");

    // Press the left button: the host pushes the MK_* mask through the mirror
    // (channel-only, no big lock) exactly as the winit handler does.
    state.present().channel.push_mouse_buttons(MK_LBUTTON);
    let (z, buttons) = read(&mut engine, &mut state);
    assert_eq!(
        &buttons[..4],
        &[0x80, 0, 0, 0],
        "the left button reads 0x80 (down) at rgbButtons[0]"
    );
    assert_eq!(z, 0, "a press does not move the wheel");
    // Buttons are LEVEL state, unlike the wheel: a second read with no new
    // host event must still report the press.
    let (_, buttons) = read(&mut engine, &mut state);
    assert_eq!(
        &buttons[..4],
        &[0x80, 0, 0, 0],
        "a polled button state is not consumed on read"
    );

    // Add the middle button, then release both.
    state
        .present()
        .channel
        .push_mouse_buttons(MK_LBUTTON | MK_MBUTTON);
    let (_, buttons) = read(&mut engine, &mut state);
    assert_eq!(
        &buttons[..4],
        &[0x80, 0, 0x80, 0],
        "the middle button is rgbButtons[2], not the MK bit's low bit"
    );
    state.present().channel.push_mouse_buttons(0);
    let (_, buttons) = read(&mut engine, &mut state);
    assert_eq!(buttons, [0_u8; 8], "releasing every button reads all-up");

    // Wheel: two whole notches down (negative) accumulate into one lZ, and the
    // report consumes them — a polled lZ is relative, like lX/lY.
    state.present().channel.push_wheel_notches(false, -1);
    state.present().channel.push_wheel_notches(false, -1);
    // A horizontal notch has no DIMOUSESTATE field, but it still arrives here
    // and must not leak into lZ.
    state.present().channel.push_wheel_notches(true, 5);
    let (z, _) = read(&mut engine, &mut state);
    assert_eq!(
        z,
        -2 * WHEEL_DELTA,
        "two notches down accumulate into one lZ of -240, horizontal excluded"
    );
    let (z, _) = read(&mut engine, &mut state);
    assert_eq!(
        z, 0,
        "lZ is consumed on read, so a poll loop sees each notch once"
    );
}
