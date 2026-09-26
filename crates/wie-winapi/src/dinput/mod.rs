//! DirectInput 8 compatibility shim (`dinput8.dll`) — keyboard + mouse only.
//!
//! WIE has no DirectInput driver stack, no HID layer, and no force-feedback
//! engine. This module is a **shim over WIE's existing keyboard and mouse
//! state** (the presenter-side `WindowMirror` and the 256-byte
//! `KeyboardState` the USER32 readers already use), not a DirectInput
//! implementation. A guest that needs "is a key down" / "how far did the mouse
//! move" gets real data; a guest that needs anything else gets an honest
//! `HRESULT` failure so it can take a fallback path.
//!
//! # What is real
//!
//! - `DirectInput8Create` hands back a real guest `IDirectInput8` COM object
//!   whose 11-slot vtable points at WIE's stop VAs (the same wiring shape as
//!   [`crate::d3d9`]).
//! - `EnumDevices` enumerates exactly two devices — a keyboard, then a mouse —
//!   through the guest's `DIDEVICEENUMCALLBACK`, using the full-iteration
//!   continuation bridge, so the guest really is called twice.
//! - `CreateDevice` returns a real `IDirectInputDevice8` COM object for those
//!   two device GUIDs; any other GUID fails honestly.
//! - `GetDeviceState` synthesizes the `DIDEVICEOBJECTDATA` array the guest's
//!   own `SetDataFormat` `DIDATAFORMAT` describes, sourced from the live
//!   keyboard/mouse state.
//! - `GetCapabilities` / `GetDeviceInfo` / `GetDeviceStatus` / `FindDevice`
//!   report those two devices honestly.
//! - `SetDataFormat`, `SetCooperativeLevel`, `Acquire`, `Unacquire`, `Poll`
//!   are accepted and stored. Cooperative-level *semantics* (exclusive,
//!   foreground/background, `DISCL_NOWINKEY`) are **not** enforced: WIE has one
//!   global keyboard/mouse and every guest sees all of it.
//!
//! # What is an honest failure, never silence
//!
//! - No joysticks, gamepads, flight/driving controls, wheels, or
//!   `DIDEVTYPE_HID` devices exist. `CreateDevice` for such a GUID returns
//!   `DIERR_INVALIDPARAM`; `GetDeviceStatus` returns `DI_NOTATTACHED`
//!   (`S_FALSE`); `EnumDevices` never lists them.
//! - No force feedback. `CreateEffect` returns `DIERR_UNSUPPORTED`
//!   (`E_NOTIMPL`), `EnumEffects` returns `DI_NOEFFECT` (`S_FALSE`).
//! - No action maps. `EnumDevicesBySemantics` returns `DI_NOTATTACHED`
//!   (`S_FALSE`).
//! - This is polled state, not a buffered event ring: `GetDeviceData` and
//!   `SendDeviceData` return `DIERR_UNSUPPORTED` (`E_NOTIMPL`).
//! - `RunControlPanel` succeeds without showing anything — there is no control
//!   panel in WIE to show.
//!
//! # Known gaps in the "real" set
//!
//! - **Mouse buttons always read up** (`DIMOUSESTATE::rgbButtons` is all
//!   zero) and **the wheel always reads 0** (`lZ`). WIE's host input seam
//!   (`GuestHandle::set_key_state` / `set_cursor_pos`) pushes *keyboard*
//!   virtual keys and the *cursor position* only — there is no mouse-button or
//!   scroll state anywhere in the runtime to read. The `DIDF_CDATAFORMAT`
//!   mouse report is therefore half real: `lX`/`lY` are genuine per-report
//!   deltas (see [`DInputDeviceRecord::last_cursor`]), the button and wheel
//!   bytes are a truthful "nothing tracked" zero. A guest that needs mouse
//!   clicks through DirectInput must go through the `WM_LBUTTONDOWN` path the
//!   host already drives.
//!
//! # Locking
//!
//! Every handler runs on a guest thread already holding the big `WinApiState`
//! mutex and reads input through that same lock
//! (`WinApiState::drain_key_writes` / `keyboard_state` / `cursor_pos`). No host
//! thread ever takes the big lock, and this module never calls a host
//! callback, so the guest-big-then-channel / host-channel-only ordering in
//! [`crate::present`] is preserved.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

mod device;
mod enumerate;
mod object;

pub(crate) use enumerate::advance_enumeration;
pub(crate) use object::dispatch_object_method;

use object::direct_input8_create;

#[cfg(test)]
pub(crate) use enumerate::{DIENUM_CONTINUE, DIENUM_STOP};

// ── HRESULT / status codes (dinput.h) ─────────────────────────────────────

/// `DI_OK` = `S_OK` (dinput.h:156).
pub(crate) const DI_OK: u64 = 0;
/// `DI_NOTATTACHED` = `S_FALSE` (dinput.h:157) — the honest "no such device /
/// no effects / no action-mapped devices" answer.
pub(crate) const DI_NOTATTACHED: u64 = 1;
/// `DIERR_UNSUPPORTED` = `E_NOTIMPL` (dinput.h:184) = `0x80004001`.
pub(crate) const DIERR_UNSUPPORTED: u64 = 0x8000_4001;
/// `DIERR_NOINTERFACE` = `E_NOINTERFACE` (dinput.h:181) = `0x80004002`.
pub(crate) const DIERR_NOINTERFACE: u64 = 0x8000_4002;
/// `DIERR_INVALIDPARAM` = `E_INVALIDARG` (dinput.h:180) = `0x80070057`.
pub(crate) const DIERR_INVALIDPARAM: u64 = 0x8007_0057;
/// `DIERR_NOTACQUIRED` (dinput.h:195-196) =
/// `MAKE_HRESULT(1, FACILITY_WIN32, ERROR_INVALID_ACCESS /* 5 */)` =
/// `0x80070005`.
pub(crate) const DIERR_NOTACQUIRED: u64 = 0x8007_0005;
/// `DIERR_OLDDIRECTINPUTVERSION` (dinput.h:169-170) =
/// `MAKE_HRESULT(1, FACILITY_WIN32, ERROR_OLD_WIN_VERSION /* 129 */)` =
/// `0x80070081`.
pub(crate) const DIERR_OLDDIRECTINPUTVERSION: u64 = 0x8007_0081;
/// `DI_NOTATTACHED` as a *failure* for a device class WIE does not have
/// (`DIERR_NOTFOUND`, dinput.h:176-177 =
/// `MAKE_HRESULT(1, FACILITY_WIN32, ERROR_FILE_NOT_FOUND /* 2 */)` =
/// `0x80070002`).
pub(crate) const DIERR_NOTFOUND: u64 = 0x8007_0002;

// ── Device identity (dinput.h) ───────────────────────────────────────────

/// `DI8DEVTYPE_KEYBOARD` (dinput.h:239).
const DI8DEVTYPE_KEYBOARD: u32 = 0x13;
/// `DIDEVTYPE_KEYBOARD` (dinput.h:227).
const DIDEVTYPE_KEYBOARD: u32 = 3;
/// `DI8DEVTYPE_MOUSE` (dinput.h:238).
const DI8DEVTYPE_MOUSE: u32 = 0x12;
/// `DIDEVTYPE_MOUSE` (dinput.h:226).
const DIDEVTYPE_MOUSE: u32 = 2;

/// `dwDevType` for the synthetic keyboard:
/// `(DI8DEVTYPE_KEYBOARD << 8) | DIDEVTYPE_KEYBOARD` = `0x00001303`.
pub(crate) const DI_DEVTYPE_KEYBOARD: u32 = (DI8DEVTYPE_KEYBOARD << 8) | DIDEVTYPE_KEYBOARD;
/// `dwDevType` for the synthetic mouse:
/// `(DI8DEVTYPE_MOUSE << 8) | DIDEVTYPE_MOUSE` = `0x00001202`.
pub(crate) const DI_DEVTYPE_MOUSE: u32 = (DI8DEVTYPE_MOUSE << 8) | DIDEVTYPE_MOUSE;

/// `DI8DEVTYPE_GAMEPAD` (dinput.h:241) — a class WIE explicitly does not have.
const DI8DEVTYPE_GAMEPAD: u32 = 0x15;
/// `DI8DEVTYPE_JOYSTICK` (dinput.h:240) — likewise absent.
const DI8DEVTYPE_JOYSTICK: u32 = 0x14;

/// The DirectInput 8 version `DirectInput8Create` requires (`0x0800`, the
/// header's own `DIRECTINPUT_VERSION` default, dinput.h:28).
pub(crate) const DIRECTINPUT_VERSION_8: u64 = 0x0800;

/// Guest allocation size for one `IDirectInput8` / `IDirectInputDevice8`
/// vtable + object pair. Comfortably above the 32-slot device vtable
/// (32 * 8 = 256 bytes) plus the object pointer.
const DINPUT_VTABLE_ALLOCATION_SIZE: u64 = 0x200;

/// Offset of the COM object after its vtable. Only needs to clear the vtable
/// with room to spare.
const DINPUT_OBJECT_OFFSET: u64 = 0x100;

/// The two devices WIE exposes, in `EnumDevices` order (keyboard, then mouse).
pub(crate) const DEVICES: [DInputDeviceClass; 2] =
    [DInputDeviceClass::Keyboard, DInputDeviceClass::Mouse];

/// The device classes WIE implements. Each is one synthetic device; nothing
/// else exists (no joysticks, gamepads, flight controls, or HID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DInputDeviceClass {
    /// One keyboard: 256 key slots, no axes, 256 pseudo-buttons.
    Keyboard,
    /// One mouse: 2 relative axes (X, Y), 4 buttons, no POV, no wheel data.
    Mouse,
}

impl DInputDeviceClass {
    /// The `dwDevType` WIE reports for this device.
    pub(crate) const fn dev_type(self) -> u32 {
        match self {
            Self::Keyboard => DI_DEVTYPE_KEYBOARD,
            Self::Mouse => DI_DEVTYPE_MOUSE,
        }
    }

    /// The `DI8DEVCLASS_*` this device belongs to (dinput.h:231-235).
    ///
    /// Exposed for diagnostics and tests: a guest that filters
    /// `EnumDevices` by class wants the *type*, not the class, so this is not
    /// used in the dispatch path — it is here so the mapping is written down
    /// once rather than re-derived by a reader.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn dev_class(self) -> u32 {
        match self {
            Self::Keyboard => 3, // DI8DEVCLASS_KEYBOARD
            Self::Mouse => 2,    // DI8DEVCLASS_POINTER
        }
    }

    /// Human-readable product name written into `DIDEVICEINSTANCEA`.
    pub(crate) const fn product_name(self) -> &'static str {
        match self {
            Self::Keyboard => "WIE Virtual Keyboard",
            Self::Mouse => "WIE Virtual Mouse",
        }
    }

    /// Human-readable instance name written into `DIDEVICEINSTANCEA`.
    pub(crate) const fn instance_name(self) -> &'static str {
        match self {
            Self::Keyboard => "WIE Virtual Keyboard",
            Self::Mouse => "WIE Virtual Mouse",
        }
    }

    /// The `DIDEVICEINSTANCEA::guidProduct` WIE synthesizes for this device.
    ///
    /// A deterministic, WIE-private value: real DirectInput product GUIDs are
    /// enumerated by the OS, and there is nothing to enumerate here. It only
    /// has to be stable across calls so a guest that looks the device up by
    /// product finds the same GUID.
    pub(crate) const fn product_guid(self) -> [u8; 16] {
        match self {
            // Data1 0xA426_D2D6, Data2 0xD016, Data3 0xC011 — version-4 UUID
            // shape, entirely made up but well-formed.
            Self::Keyboard => [
                0xd6, 0xd2, 0x26, 0xa4, 0x16, 0xd0, 0x11, 0xc0, 0x8a, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x01,
            ],
            Self::Mouse => [
                0xd7, 0xd2, 0x26, 0xa4, 0x16, 0xd0, 0x11, 0xc0, 0x8a, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x02,
            ],
        }
    }

    /// The `DIDEVICEINSTANCEA::guidInstance` WIE synthesizes: the product GUID
    /// with its last byte replaced by the device ordinal, so keyboard and mouse
    /// instance GUIDs differ (a guest uses `guidInstance` for
    /// `CreateDevice`/`GetDeviceStatus`).
    pub(crate) fn instance_guid(self) -> [u8; 16] {
        let mut guid = self.product_guid();
        let ordinal = match self {
            Self::Keyboard => 0x11_u8,
            Self::Mouse => 0x22,
        };
        if let Some(last) = guid.last_mut() {
            *last = ordinal;
        }
        guid
    }

    /// `DIDEVCAPS::dwAxes` WIE reports.
    pub(crate) const fn axes(self) -> u32 {
        match self {
            // A keyboard has no analog axes at all.
            Self::Keyboard => 0,
            Self::Mouse => 2,
        }
    }

    /// `DIDEVCAPS::dwButtons` WIE reports. The keyboard's 256 keys are exposed
    /// as pseudo-buttons (that is what `c_dfDIKeyboard` does); the mouse
    /// reports the 4 buttons its `DIMOUSESTATE` carries.
    pub(crate) const fn buttons(self) -> u32 {
        match self {
            Self::Keyboard => 256,
            Self::Mouse => 4,
        }
    }

    /// Whether an `EnumDevices` / `GetDeviceStatus` `dwDevType` filter selects
    /// this device.
    ///
    /// DirectInput accepts three spellings of the filter, and the
    /// `DI8DEVCLASS_*` / `DIDEVTYPE_*` values were deliberately chosen to
    /// coincide (dinput.h:225-235), so all of them are honoured:
    ///
    /// - `0` (`DI8DEVCLASS_ALL`) or `1` (`DI8DEVCLASS_DEVICE`) — every device.
    /// - A bare `DI8DEVCLASS_*` / `DIDEVTYPE_*` in `2..=4`: the low byte, which
    ///   for WIE's two devices is 2 (pointer/mouse) and 3 (keyboard).
    /// - A bare `DI8DEVTYPE_*` in `0x11..=0x1d`: the high byte.
    /// - A full `dwDevType` = `(DI8DEVTYPE_* << 8) | DIDEVTYPE_*`.
    ///
    /// `4` (`DI8DEVCLASS_GAMECTRL`) and every `DIDEVTYPE_JOYSTICK` /
    /// `DIDEVTYPE_HID` spelling therefore match neither device, which is what
    /// makes `EnumDevices` complete with no callback for a guest probing for a
    /// joystick or a gamepad.
    pub(crate) fn matches_dev_type(self, dev_type: u64) -> bool {
        const DI8DEVCLASS_ALL: u64 = 0;
        const DI8DEVCLASS_DEVICE: u64 = 1;
        if dev_type == DI8DEVCLASS_ALL || dev_type == DI8DEVCLASS_DEVICE {
            return true;
        }
        let our_type = u64::from(self.dev_type());
        let value = u64::from(u32::try_from(dev_type).unwrap_or(u32::MAX));
        value == (our_type & 0xff) // DI8DEVCLASS_* / DIDEVTYPE_*
            || value == (our_type >> 8) // DI8DEVTYPE_*
            || value == our_type // full dwDevType
    }

    /// The device this instance GUID names, or `None` for a GUID WIE does not
    /// have (a real product GUID from a real machine, say).
    pub(crate) fn from_instance_guid(guid: &[u8]) -> Option<Self> {
        DEVICES
            .into_iter()
            .find(|class| class.instance_guid() == guid)
    }

    /// The device this product GUID names, or `None`.
    pub(crate) fn from_product_guid(guid: &[u8]) -> Option<Self> {
        DEVICES
            .into_iter()
            .find(|class| class.product_guid() == guid)
    }
}

/// The `DI8DEVTYPE_*` classes WIE deliberately has no device for, named here so
/// the not-implemented list in the module docs is a checked constant rather
/// than prose that can drift from the code.
///
/// These are what a guest passes as `dwDevType` when it is looking for a
/// joystick or a gamepad: `EnumDevices` filters both out (the walk completes
/// with no callback) and `CreateDevice` cannot name them (no such GUID is
/// synthesized), so a guest probing for them gets "none" rather than silence.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const UNSUPPORTED_DEVTYPES: [u32; 2] = [DI8DEVTYPE_JOYSTICK, DI8DEVTYPE_GAMEPAD];

/// One live `IDirectInputDevice8` object.
#[derive(Debug)]
pub(crate) struct DInputDeviceRecord {
    /// Guest address of the COM object (what the guest passes as `this`).
    pub(crate) object_address: u64,
    /// Which synthetic device this is.
    pub(crate) class: DInputDeviceClass,
    /// `IUnknown` refcount.
    pub(crate) ref_count: u64,
    /// The `DIDATAFORMAT` the guest handed to `SetDataFormat`, copied out with
    /// its `DIOBJECTDATAFORMAT` array. `None` until `SetDataFormat` runs —
    /// `GetDeviceState` fails with `DIERR_NOTINITIALIZED` until then, which is
    /// exactly what a real DirectInput8 device does.
    pub(crate) data_format: Option<GuestDataFormat>,
    /// `SetCooperativeLevel`'s `HWND`.
    pub(crate) cooperative_hwnd: u64,
    /// `SetCooperativeLevel`'s `DISCL_*` flags, stored but not enforced.
    pub(crate) cooperative_flags: u32,
    /// `Acquire` / `Unacquire` bookkeeping. Not enforced either: WIE has one
    /// global keyboard/mouse, so a guest can read state whether or not it
    /// acquired.
    pub(crate) acquired: bool,
    /// Cursor position this device last reported, so the next `GetDeviceState`
    /// can turn the mirror's level-triggered absolute position into the
    /// relative `lX`/`lY` a `DIDF_CDATAFORMAT` mouse report wants. `None`
    /// until the first read (which therefore reports a 0 delta).
    pub(crate) last_cursor: Option<(i32, i32)>,
    /// Monotonic `DIDEVICEOBJECTDATA::dwSequence` counter.
    pub(crate) sequence: u32,
}

impl DInputDeviceRecord {
    pub(crate) fn new(object_address: u64, class: DInputDeviceClass) -> Self {
        Self {
            object_address,
            class,
            ref_count: 1,
            data_format: None,
            cooperative_hwnd: 0,
            cooperative_flags: 0,
            acquired: false,
            last_cursor: None,
            sequence: 0,
        }
    }
}

/// A guest `DIDATAFORMAT` copied out of guest memory, with its object array.
///
/// Only the three fields the read path needs survive the copy: `dwDataSize`
/// (how much report space the guest promised) and the object array. `dwSize`,
/// `dwObjSize`, `dwFlags`, and `dwNumObjs` are validated in
/// `set_data_format` against the same locals it read them into, and keeping
/// second copies of them here would be a struct that can disagree with itself.
#[derive(Debug, Clone)]
pub(crate) struct GuestDataFormat {
    /// `DIDATAFORMAT::dwDataSize` — total report size in bytes.
    pub(crate) data_size: u32,
    /// The `DIOBJECTDATAFORMAT` array, copied (never a guest pointer, so the
    /// guest may free it right after `SetDataFormat` returns).
    pub(crate) objects: Vec<crate::guest_layout::DiObjectDataFormat>,
}

/// DirectInput8 host state: the `IDirectInput8` object and its devices.
#[derive(Debug, Default)]
pub struct DInputState {
    /// Guest address of the live `IDirectInput8` object (0 before
    /// `DirectInput8Create` runs).
    pub(crate) direct_input_object: u64,
    /// Its `IUnknown` refcount.
    pub(crate) ref_count: u64,
    /// Every `CreateDevice`'d device, in creation order.
    pub(crate) devices: Vec<DInputDeviceRecord>,
}

impl DInputState {
    /// The device record for guest object address `this`, if any.
    pub(crate) fn device(&self, this: u64) -> Option<&DInputDeviceRecord> {
        self.devices
            .iter()
            .find(|device| device.object_address == this)
    }

    /// The device record for guest object address `this`, mutably.
    pub(crate) fn device_mut(&mut self, this: u64) -> Option<&mut DInputDeviceRecord> {
        self.devices
            .iter_mut()
            .find(|device| device.object_address == this)
    }
}

/// Dispatch one `dinput8.dll` export by name (case-insensitive).
///
/// `dinput8.dll` has exactly one real export, `DirectInput8Create` (also
/// ordinal 1, and also reachable through `GetProcAddress`). Every other
/// DirectInput entry point arrives as a `FakeVa::Com` vtable slot, routed by
/// [`crate::fake_va::ComIface::library`] to this same library name and then
/// dispatched by [`object::dispatch_object_method`] /
/// [`device::dispatch_device_method`] on the `IDirectInput*::Xxx` trace name.
pub fn dispatch_dinput8(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    if name.eq_ignore_ascii_case("DirectInput8Create") {
        return Ok(Some(direct_input8_create(ctx)?));
    }
    Ok(None)
}
