//! USER32 RawInput lane (soft-dispatch): `RegisterRawInputDevices`,
//! `GetRegisteredRawInputDevices`, `GetRawInputDeviceList`,
//! `GetRawInputDeviceInfoA/W`, `GetRawInputBuffer`, `GetRawInputData`, and
//! `DefRawInputProc`, plus the [`RawInputState`] the host lane feeds.
//!
//! Routed from `dispatch_user32_extra` (user32/mod.rs) — the string-match
//! fallback the dense `WinApiId` table does not cover. RawInput is not a
//! hot-path API in WIE (see the wiring note in `user32/mod.rs`).
//!
//! # The payloads are SYNTHESIZED, not real hardware raw input
//!
//! macOS exposes no raw HID stream to a normal app and winit has no portable
//! raw-input event, so WIE has no source of true `RAWINPUT` reports. The
//! records this module hands out are **synthesized** from the keyboard/mouse
//! state WIE already tracks — the same compromise Wine makes when it runs on a
//! platform with no raw input. Concretely:
//!
//! * A guest that registers for raw input and walks the buffer gets plausible,
//!   correctly laid-out records — never real scan codes from real hardware.
//!   `RAWKEYBOARD.MakeCode` is therefore always 0: winit reports no scancode.
//! * Two devices exist: [`SYNTHESIZED_MOUSE_DEVICE`] and
//!   [`SYNTHESIZED_KEYBOARD_DEVICE`]. There is no HID device, so
//!   `RIDI_PREPARSEDDATA` fails and `RIM_TYPEHID` never appears in a record.
//! * Button and wheel records report the CLIENT-relative logical position in
//!   `lLastX`/`lLastY` with `MOUSE_MOVE_ABSOLUTE`, not Windows' desktop-
//!   normalized 0..65535 space (WIE has no such space). Movement records use
//!   `MOUSE_MOVE_RELATIVE` with a true per-event delta, which is exact.
//! * `DefRawInputProc` forwards at most one record and cannot propagate the
//!   guest proc's `LRESULT` (see that handler).
//!
//! Do not describe this lane as hardware raw input anywhere: it is a
//! synthesized stand-in, and a guest must not be able to tell the difference
//! by any means other than `MakeCode` and the device paths.
//!
//! # Delivery: `WM_INPUT` posts
//!
//! [`post_raw_keyboard_event`] / [`post_raw_mouse_event`] are the host entry
//! points. They apply the registration filter (`RIDEV_*`), queue a synthesized
//! record, and return the [`RawInputPost`]s the host must send — `wParam` is
//! `GET_RAWINPUT_CODE_WPARAM`, `lParam` the record's `HRAWINPUT`. The host
//! posts those through its own per-thread message routing
//! (`GuestHandle::post_message` → `MessageQueue::push_host`); this module
//! never posts a message itself, because only the host knows the window-owner
//! thread routing.
//!
//! The `lParam` is an opaque **fake handle**, not a guest address — exactly
//! like Windows, where `HRAWINPUT` (winuser.h:6291) is a system handle the
//! guest must pass back to `GetRawInputData` rather than dereference. The
//! record bytes live in [`RawInputState::delivered`], which
//! [`handle_get_raw_input_data`] resolves; the same handler still falls back
//! to reading guest memory, which is how a record inside a `GetRawInputBuffer`
//! fill is read back.
//!
//! # Two views of ONE buffered input
//!
//! On Windows the system buffers raw input per input queue, and the
//! `WM_INPUT` `lParam` `HRAWINPUT` and [`handle_get_raw_input_buffer`] are two
//! views of that same buffer: a guest may read either, in any order. WIE keeps
//! that property with two stores fed from one place:
//!
//! * [`RawInputState::delivered`] — keyed by the fake `HRAWINPUT`, capped at
//!   [`DELIVERED_CAP`], and **never consumed**: this is what
//!   `GetRawInputData(lParam)` resolves, so a `WM_INPUT` `lParam` stays
//!   readable for as long as the guest holds it.
//! * [`RawInputState::buffered`] — the FIFO `GetRawInputBuffer` serialises from
//!   and **drains**, capped at [`BUFFERED_CAP`]. A successful fill consumes its
//!   records, exactly as Windows consumes the calling thread's queued input.
//!
//! [`RawInputState::publish`] is the single feeder of both, so a record is never
//! in one store and not the other. A `GetRawInputBuffer` drain therefore cannot
//! invalidate a `WM_INPUT` `lParam` the guest is still holding: the two views
//! have independent lifetimes by design, and no record is double-counted — the
//! return value is the count of records taken from the buffer view only.
//!
//! The buffer view is [`RawInputState::pending`] **plus**
//! [`RawInputState::buffered`]: a record is buffered from the moment the host
//! synthesizes it, not from the moment its `WM_INPUT` is posted, so a record a
//! caller queued itself (via [`enqueue_keyboard`] / [`enqueue_mouse`]) and
//! never drained is still visible to `GetRawInputBuffer`.
//!
//! # Deviation: `GetRawInputBuffer` is not per-thread
//!
//! Windows buffers raw input per input queue and `GetRawInputBuffer` returns
//! only the calling thread's queued input. **WIE has no per-thread raw-input
//! attribution at all**: [`RawInputState`] is one process-wide queue behind one
//! `Mutex` (see below), and [`post_raw_keyboard_event`] /
//! [`post_raw_mouse_event`] are fed by the single winit event thread. Rather
//! than invent a threading model to fix one function, `GetRawInputBuffer`
//! serves the whole process-wide queue — the least-wrong reading of "the input
//! the system buffered for you" — so a guest sees every synthesized record
//! instead of an arbitrary thread's share. A multi-threaded guest that mixes
//! `GetRawInputBuffer` across threads can therefore see a record the other
//! thread also sees; every other part of the lane (registration filtering,
//! `WM_INPUT` routing, `GetRawInputData`) keeps its per-window / per-handle
//! scoping. Fixing this properly means threading the input queue's owner down
//! from the host event path, which is a separate change.
//!
//! # The structures are NOT synthesized
//!
//! Every layout is the real Win64 one (see `crate::guest_layout`), because
//! guests re-walk them with `NEXTRAWINPUTBLOCK` and read them field by field.
//!
//! # Why the state is a process-global `Mutex`
//!
//! The host-delivery path runs on the winit thread, which must not take the big
//! `WinApiState` lock that a guest thread holds for the whole duration of an
//! API call — the same reason `WinApiState::message_queue` is a separate
//! `Arc<Mutex<_>>`. A module-local `Mutex` is also the established shape for
//! string-dispatched USER32 state in this tree (see `enum_caret`'s `CARET_*`
//! statics). A poisoned lock is recovered with `into_inner` rather than
//! unwinding through a guest call.

use std::sync::Mutex;

use zerocopy::IntoBytes;

use super::{
    Context, GuestCallbackRequest, HandlerContext, OuterReturn, Result, WinApiControlSignal,
    WinApiHandlerResult, checked_address, read_guest_bytes, read_u32, with_typed_read,
    with_typed_write, write_guest_ansi_c_string, write_guest_bytes, write_guest_u32,
    write_guest_utf16_c_string,
};
use crate::gdi32::{ArgReg, read_arg};

/// The guest-visible RawInput layouts and constants live with every other
/// guest struct in `crate::guest_layout` (their const-assert drift tables and
/// raw-byte tests are there too). Re-exported so the handlers in this module
/// name one module for the whole RawInput surface; a test that only needs a
/// constant or a layout should import it from `crate::guest_layout` directly,
/// so this list stays exactly what the handlers themselves reference.
pub(crate) use crate::guest_layout::{
    HID_USAGE_GENERIC_KEYBOARD, HID_USAGE_GENERIC_MOUSE, HID_USAGE_PAGE_GENERIC,
    RAW_INPUT_DEVICE_LIST_SIZE, RAW_INPUT_DEVICE_SIZE, RAW_INPUT_HEADER_SIZE, RAW_KEYBOARD_SIZE,
    RAW_MOUSE_SIZE, RID_DEVICE_INFO_SIZE, RID_HEADER, RID_INPUT, RIDEV_EXCLUDE, RIDEV_EXMODEMASK,
    RIDEV_INPUTSINK, RIDEV_PAGEONLY, RIDEV_REMOVE, RIDI_DEVICEINFO, RIDI_DEVICENAME, RIM_INPUT,
    RIM_INPUTSINK, RIM_TYPE_KEYBOARD, RIM_TYPE_MOUSE, RawInputDevice, RawInputDeviceList,
    RawInputHeader, RawKeyboard, RawMouse, RidDeviceInfo, RidDeviceInfoHid, RidDeviceInfoKeyboard,
    RidDeviceInfoMouse,
};

/// Win32 `ERROR_INVALID_PARAMETER`.
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
/// Win32 `ERROR_INSUFFICIENT_BUFFER`.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// The `HANDLE` `GetRawInputDeviceList` reports for the synthesized mouse.
pub const SYNTHESIZED_MOUSE_DEVICE: u64 = 0x0000_0000_6600_0502;
/// The `HANDLE` `GetRawInputDeviceList` reports for the synthesized keyboard.
pub const SYNTHESIZED_KEYBOARD_DEVICE: u64 = 0x0000_0000_6600_0501;

/// Cap on a `RIDI_DEVICENAME` round trip, in characters. The synthesized names
/// are ~20 units; the cap only bounds a hostile guest that passes a huge
/// `*pcbSize`.
const DEVICE_NAME_MAX: usize = 1024;

/// Base of the fake `HRAWINPUT` handle space. A delivered record's `lParam` is
/// `HRAWINPUT_BASE + n` — a `HANDLE`-shaped value outside every WIE VA range,
/// exactly like the `FAKE_HDROP` `WM_DROPFILES` uses. Never dereferenceable by
/// a guest: `GetRawInputData` resolves it against
/// [`RawInputState::delivered`].
const HRAWINPUT_BASE: u64 = 0x0000_0000_6600_0600;

/// How many delivered records stay resolvable by `GetRawInputData`.
///
/// A guest reads its `HRAWINPUT` while handling the `WM_INPUT` that carried
/// it, so a handful of records is plenty; the cap only bounds a host that
/// delivers faster than the guest pumps (a stuck guest must not grow this
/// without limit). The oldest record is dropped first — a stale `lParam` then
/// reads as a normal "bad handle" failure, which is the same outcome Windows
/// produces for a handle it has already reclaimed.
const DELIVERED_CAP: usize = 512;

/// How many delivered records stay in the `GetRawInputBuffer` queue.
///
/// A raw-input guest is expected to drain that queue promptly, but a guest that
/// only ever reads `GetRawInputData(lParam)` never calls it, so the queue has to
/// be bounded independently of [`DELIVERED_CAP`]. The oldest record is dropped
/// first, which a guest sees as a short `*pcbSize` rather than an error.
const BUFFERED_CAP: usize = 512;

/// The synthesized device path string WIE reports for the mouse. Not a real
/// macOS device path — see the module-level synthesis note.
const MOUSE_DEVICE_PATH: &str = "\\\\?\\WIE\\SYNTH\\HID\\MOUSE";
/// The synthesized device path string WIE reports for the keyboard.
const KEYBOARD_DEVICE_PATH: &str = "\\\\?\\WIE\\SYNTH\\HID\\KEYBOARD";

/// The payload half of one synthesized `RAWINPUT` record.
#[derive(Debug, Clone)]
pub(crate) enum RawInputPayload {
    /// A synthesized `RAWMOUSE` report.
    Mouse(RawMouse),
    /// A synthesized `RAWKEYBOARD` report.
    Keyboard(RawKeyboard),
}

impl RawInputPayload {
    /// `RAWINPUTHEADER.dwType` for this payload.
    #[must_use]
    pub(crate) fn device_type(&self) -> u32 {
        match self {
            Self::Mouse(_) => RIM_TYPE_MOUSE,
            Self::Keyboard(_) => RIM_TYPE_KEYBOARD,
        }
    }

    /// The payload's own byte size (the union member alone, header excluded).
    #[must_use]
    pub(crate) fn payload_size(&self) -> u32 {
        match self {
            Self::Mouse(_) => RAW_MOUSE_SIZE,
            Self::Keyboard(_) => RAW_KEYBOARD_SIZE,
        }
    }

    /// Append the payload's raw bytes to `out` (the packing path).
    fn append_bytes(&self, out: &mut Vec<u8>) {
        match self {
            Self::Mouse(mouse) => out.extend_from_slice(mouse.as_bytes()),
            Self::Keyboard(keyboard) => out.extend_from_slice(keyboard.as_bytes()),
        }
    }
}

/// One synthesized record waiting to be delivered to a window.
#[derive(Debug, Clone)]
pub(crate) struct PendingRawInput {
    /// The window the record is destined for; `0` means input-sink delivery
    /// (`RIDEV_INPUTSINK` with a NULL `hwndTarget`).
    pub(crate) target: u64,
    /// The `RAWINPUTHEADER`, with `dwSize` already set to the packed record
    /// size and `wParam` carrying the `RIM_INPUT` / `RIM_INPUTSINK` code.
    pub(crate) header: RawInputHeader,
    /// The payload.
    pub(crate) payload: RawInputPayload,
}

impl PendingRawInput {
    /// The packed record size: `sizeof(RAWINPUTHEADER)` + the payload, with no
    /// per-record alignment padding (winuser.h:6403's `RAWINPUT_ALIGN` is a
    /// no-op at both 40 and 48 on x64).
    #[must_use]
    pub(crate) fn packed_size(&self) -> u32 {
        RAW_INPUT_HEADER_SIZE.saturating_add(self.payload.payload_size())
    }

    /// `GET_RAWINPUT_CODE_WPARAM(wParam)` (winuser.h:6294) — `RIM_INPUT` for
    /// a foreground window, `RIM_INPUTSINK` for a background-only one.
    #[must_use]
    pub(crate) fn wparam_code(&self) -> u32 {
        if self.target == 0 {
            RIM_INPUTSINK
        } else {
            RIM_INPUT
        }
    }

    /// Append the packed record (`RAWINPUTHEADER` + payload, unaligned) to
    /// `out`, and return the number of bytes appended.
    pub(crate) fn append_packed(&self, out: &mut Vec<u8>) -> u32 {
        let start = out.len();
        out.extend_from_slice(self.header.as_bytes());
        self.payload.append_bytes(out);
        u32::try_from(out.len() - start).unwrap_or(u32::MAX)
    }
}

/// One synthesized raw-input device, as `GetRawInputDeviceList` and
/// `GetRawInputDeviceInfo` report it.
#[derive(Debug, Clone)]
pub(crate) struct RawInputDeviceRecord {
    /// The `HANDLE` the guest sees.
    pub(crate) handle: u64,
    /// `RIM_TYPEMOUSE` / `RIM_TYPEKEYBOARD` / `RIM_TYPEHID`.
    pub(crate) device_type: u32,
    /// The `RIDI_DEVICENAME` path string.
    pub(crate) name: String,
    /// The `RID_DEVICE_INFO` mouse member.
    pub(crate) mouse: RidDeviceInfoMouse,
    /// The `RID_DEVICE_INFO` keyboard member.
    pub(crate) keyboard: RidDeviceInfoKeyboard,
    /// The `RID_DEVICE_INFO` HID member.
    pub(crate) hid: RidDeviceInfoHid,
}

impl RawInputDeviceRecord {
    /// The two devices WIE always reports. Fixed and ordered mouse-first, so
    /// `GetRawInputDeviceList` is stable across calls.
    #[must_use]
    pub(crate) fn synthesized() -> Vec<Self> {
        vec![
            Self {
                handle: SYNTHESIZED_MOUSE_DEVICE,
                device_type: RIM_TYPE_MOUSE,
                name: MOUSE_DEVICE_PATH.to_string(),
                mouse: RidDeviceInfoMouse {
                    id: 1,
                    number_of_buttons: 5,
                    sample_rate: 0,
                    has_horizontal_wheel: 1,
                },
                keyboard: RidDeviceInfoKeyboard {
                    keyboard_type: 0,
                    keyboard_sub_type: 0,
                    keyboard_mode: 0,
                    number_of_function_keys: 0,
                    number_of_indicators: 0,
                    number_of_keys_total: 0,
                },
                hid: RidDeviceInfoHid {
                    vendor_id: 0x0000,
                    product_id: 0x0000,
                    version_number: 0x0000,
                    usage_page: HID_USAGE_GENERIC_MOUSE,
                    usage: HID_USAGE_GENERIC_MOUSE,
                },
            },
            Self {
                handle: SYNTHESIZED_KEYBOARD_DEVICE,
                device_type: RIM_TYPE_KEYBOARD,
                name: KEYBOARD_DEVICE_PATH.to_string(),
                mouse: RidDeviceInfoMouse {
                    id: 0,
                    number_of_buttons: 0,
                    sample_rate: 0,
                    has_horizontal_wheel: 0,
                },
                keyboard: RidDeviceInfoKeyboard {
                    keyboard_type: 4,
                    keyboard_sub_type: 0,
                    keyboard_mode: 0,
                    number_of_function_keys: 12,
                    number_of_indicators: 3,
                    number_of_keys_total: 104,
                },
                hid: RidDeviceInfoHid {
                    vendor_id: 0x0000,
                    product_id: 0x0000,
                    version_number: 0x0000,
                    usage_page: HID_USAGE_GENERIC_KEYBOARD,
                    usage: HID_USAGE_GENERIC_KEYBOARD,
                },
            },
        ]
    }
}

/// A window's raw-input registrations, in registration order.
#[derive(Debug, Clone)]
pub(crate) struct RawInputRegistration {
    /// The window (or `0` for an input-sink registration).
    pub(crate) target: u64,
    /// The guest `RAWINPUTDEVICE` as registered.
    pub(crate) device: RawInputDevice,
}

/// One delivered record, addressable by its fake `HRAWINPUT`.
#[derive(Debug, Clone)]
struct DeliveredRawInput {
    /// The fake `HRAWINPUT` a `WM_INPUT` `lParam` carries.
    handle: u64,
    /// The record header, kept beside the bytes so `GetRawInputData` can
    /// validate `dwType`/`dwSize` without a guest-memory round trip.
    header: RawInputHeader,
    /// The complete packed record (`RAWINPUTHEADER` + payload, unaligned).
    bytes: Vec<u8>,
}

/// RawInput state: per-window registrations, the pending synthesized records,
/// the delivered-record store, the `GetRawInputBuffer` queue, and the
/// synthesized device table.
///
/// See the module docs for why this lives behind a module-local `Mutex`, and for
/// how the two record stores are two views of one buffered input.
#[derive(Debug, Default)]
pub(crate) struct RawInputState {
    /// Registrations in registration order (Windows order for
    /// `GetRegisteredRawInputDevices`).
    registrations: Vec<RawInputRegistration>,
    /// Pending records in arrival order — synthesized but not yet handed to the
    /// host as a `WM_INPUT`. Still part of the `GetRawInputBuffer` queue.
    pending: Vec<PendingRawInput>,
    /// Records already handed to the host as a `WM_INPUT` `lParam`, keyed by
    /// their fake `HRAWINPUT`. This is the `GetRawInputData` view; it is never
    /// consumed.
    delivered: Vec<DeliveredRawInput>,
    /// Records posted as `WM_INPUT` but not yet taken by `GetRawInputBuffer`.
    /// This is the `GetRawInputBuffer` view of the same input; a successful
    /// fill drains it.
    buffered: Vec<PendingRawInput>,
    /// Next fake `HRAWINPUT` to hand out.
    next_handle: u64,
    /// The synthesized device table, built lazily.
    devices: Vec<RawInputDeviceRecord>,
}

/// The process-wide RawInput state.
static RAW_INPUT: Mutex<RawInputState> = Mutex::new(RawInputState {
    registrations: Vec::new(),
    pending: Vec::new(),
    delivered: Vec::new(),
    buffered: Vec::new(),
    next_handle: 0,
    devices: Vec::new(),
});

/// The process-wide [`RawInputState`].
///
/// A poisoned lock is recovered (`into_inner`) rather than unwinding through a
/// guest call — the state is plain data with no invariant a panic could break.
#[must_use]
pub(crate) fn raw_input_state() -> &'static Mutex<RawInputState> {
    &RAW_INPUT
}

impl RawInputState {
    /// Register (or unregister) one device class.
    ///
    /// `RIDEV_REMOVE` drops every matching `(usage_page, usage)` registration
    /// for the record's target; Windows ignores `hwndTarget` for a removal, so
    /// a NULL target removes the class for every window. Without the flag an
    /// identical registration is replaced in place, which is what Windows does
    /// when a window re-registers the same class.
    pub(crate) fn register(&mut self, device: RawInputDevice) {
        if device.flags & RIDEV_REMOVE != 0 {
            self.registrations.retain(|registration| {
                !(registration.target == device.target_window
                    && registration.device.usage_page == device.usage_page
                    && registration.device.usage == device.usage)
            });
            return;
        }
        if let Some(existing) = self.registrations.iter_mut().find(|registration| {
            registration.target == device.target_window
                && registration.device.usage_page == device.usage_page
                && registration.device.usage == device.usage
        }) {
            existing.device = device;
            return;
        }
        self.registrations.push(RawInputRegistration {
            target: device.target_window,
            device,
        });
    }

    /// Every current registration, in registration order.
    #[must_use]
    pub(crate) fn registrations(&self) -> &[RawInputRegistration] {
        &self.registrations
    }

    /// Whether `hwnd` registered the `(usage_page, usage)` class.
    ///
    /// A `RIDEV_PAGEONLY` registration is a page-level mask: it covers every
    /// usage on its `usUsagePage` (winuser.h:6469).
    #[must_use]
    pub(crate) fn is_registered(&self, hwnd: u64, usage_page: u16, usage: u16) -> bool {
        self.registrations
            .iter()
            .any(|registration| registration_covers(&registration.device, hwnd, usage_page, usage))
    }

    /// Whether `hwnd` asked for the legacy `WM_MOUSE*` / `WM_KEY*` messages of
    /// the `(usage_page, usage)` class to be suppressed (`RIDEV_EXCLUDE`).
    #[must_use]
    pub(crate) fn is_excluded(&self, hwnd: u64, usage_page: u16, usage: u16) -> bool {
        self.registrations.iter().any(|registration| {
            registration_covers(&registration.device, hwnd, usage_page, usage)
                && registration.device.flags & RIDEV_EXMODEMASK == RIDEV_EXCLUDE
        })
    }

    /// Whether the class is registered for input-sink-only delivery.
    #[must_use]
    pub(crate) fn is_input_sink(&self, hwnd: u64, usage_page: u16, usage: u16) -> bool {
        self.registrations.iter().any(|registration| {
            registration_covers(&registration.device, hwnd, usage_page, usage)
                && registration.device.flags & RIDEV_INPUTSINK != 0
        })
    }

    /// Queue a synthesized keyboard record for `target` (`0` = input sink).
    ///
    /// The host-delivery entry point is [`post_raw_keyboard_event`], which
    /// applies the registration filter around this.
    pub(crate) fn enqueue_keyboard(&mut self, target: u64, keyboard: RawKeyboard) {
        let payload = RawInputPayload::Keyboard(keyboard);
        self.push(target, payload);
    }

    /// Queue a synthesized mouse record for `target` (`0` = input sink).
    ///
    /// The host-delivery entry point is [`post_raw_mouse_event`], which
    /// applies the registration filter around this.
    pub(crate) fn enqueue_mouse(&mut self, target: u64, mouse: RawMouse) {
        let payload = RawInputPayload::Mouse(mouse);
        self.push(target, payload);
    }

    fn push(&mut self, target: u64, payload: RawInputPayload) {
        let device = self.device_for(payload.device_type());
        let size = RAW_INPUT_HEADER_SIZE.saturating_add(payload.payload_size());
        let wparam = if target == 0 {
            RIM_INPUTSINK
        } else {
            RIM_INPUT
        };
        self.pending.push(PendingRawInput {
            target,
            header: RawInputHeader {
                device_type: payload.device_type(),
                size,
                device,
                wparam: u64::from(wparam),
            },
            payload,
        });
    }

    /// The synthesized `HANDLE` for a `RIM_TYPE*` code.
    #[must_use]
    pub(crate) fn device_for(&self, device_type: u32) -> u64 {
        match device_type {
            RIM_TYPE_MOUSE => SYNTHESIZED_MOUSE_DEVICE,
            _ => SYNTHESIZED_KEYBOARD_DEVICE,
        }
    }

    /// The synthesized device table, built on first use.
    pub(crate) fn devices(&mut self) -> &[RawInputDeviceRecord] {
        if self.devices.is_empty() {
            self.devices = RawInputDeviceRecord::synthesized();
        }
        &self.devices
    }

    /// Look a synthesized device up by `HANDLE`.
    #[must_use]
    pub(crate) fn device(&mut self, handle: u64) -> Option<RawInputDeviceRecord> {
        self.devices()
            .iter()
            .find(|device| device.handle == handle)
            .cloned()
    }

    /// Remove and return the pending records destined for `hwnd`.
    ///
    /// The host `WM_INPUT` path uses [`post_raw_keyboard_event`] /
    /// [`post_raw_mouse_event`], which drain through [`Self::publish`] so each
    /// record gets a resolvable `HRAWINPUT`.
    pub(crate) fn take_pending_for_window(&mut self, hwnd: u64) -> Vec<PendingRawInput> {
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(self.pending.len());
        for record in self.pending.drain(..) {
            if record.target == hwnd {
                taken.push(record);
            } else {
                kept.push(record);
            }
        }
        self.pending = kept;
        taken
    }

    /// Hand a record to the host: pack it, remember it under a fresh fake
    /// `HRAWINPUT`, queue it for `GetRawInputBuffer`, and return the bytes plus
    /// that handle.
    ///
    /// Publishing is what makes `GetRawInputData(lParam, …)` resolvable, so
    /// every path that produces a `WM_INPUT` `lParam` must go through here. It
    /// is also the only feeder of the `GetRawInputBuffer` queue, which is why
    /// the two views of a record can never disagree about whether it was
    /// buffered.
    fn publish(&mut self, record: &PendingRawInput) -> RawInputRecordBytes {
        let mut bytes = Vec::new();
        record.append_packed(&mut bytes);
        let handle = self.alloc_handle();
        if self.delivered.len() >= DELIVERED_CAP {
            self.delivered.remove(0);
        }
        self.delivered.push(DeliveredRawInput {
            handle,
            header: record.header,
            bytes: bytes.clone(),
        });
        if self.buffered.len() >= BUFFERED_CAP {
            self.buffered.remove(0);
        }
        self.buffered.push(record.clone());
        RawInputRecordBytes {
            target: record.target,
            device_type: record.header.device_type,
            device: record.header.device,
            wparam_code: record.wparam_code(),
            raw_handle: handle,
            bytes,
        }
    }

    /// The record a `HRAWINPUT` names, if it is still held.
    fn delivered(&self, handle: u64) -> Option<&DeliveredRawInput> {
        self.delivered.iter().find(|record| record.handle == handle)
    }

    /// Next fake `HRAWINPUT`. Saturating, so a wrap cannot alias a live record.
    fn alloc_handle(&mut self) -> u64 {
        let handle = HRAWINPUT_BASE.saturating_add(self.next_handle);
        self.next_handle = self.next_handle.saturating_add(1);
        handle
    }

    /// Remove and return every record the `GetRawInputBuffer` queue holds,
    /// across all targets: the not-yet-posted [`pending`] records first, then
    /// the posted-but-not-yet-drained [`buffered`] ones, each group in arrival
    /// order.
    ///
    /// `GetRawInputBuffer` takes no window argument, so this is the surface that
    /// call uses. Windows scopes it to the calling thread's input queue; WIE
    /// has no per-thread attribution and serves the whole process-wide queue
    /// (see the module's "Deviation" note). It does NOT touch [`delivered`]:
    /// draining the buffer must not invalidate a `WM_INPUT` `lParam` the guest
    /// is still holding.
    ///
    /// [`pending`]: Self::pending
    /// [`buffered`]: Self::buffered
    /// [`delivered`]: Self::delivered
    pub(crate) fn take_all_buffered(&mut self) -> Vec<PendingRawInput> {
        let mut taken = std::mem::take(&mut self.pending);
        taken.append(&mut self.buffered);
        taken
    }

    /// The byte size `GetRawInputBuffer` would need for the records it would
    /// hand back: the sum of the packed record sizes, with no inter-record
    /// padding.
    ///
    /// Non-destructive: a size query or an under-sized buffer must NOT consume
    /// the records, so the packer asks this first and only drains via
    /// [`Self::take_all_buffered`] on the path that actually delivers.
    pub(crate) fn buffered_size(&self) -> u32 {
        self.pending
            .iter()
            .chain(self.buffered.iter())
            .fold(0_u32, |total, record| {
                total.saturating_add(record.packed_size())
            })
    }

    /// Drop every registration, pending record, delivered record, buffered
    /// record, and cached device.
    ///
    /// Test-only today (the per-session reset the host lane will need lands
    /// with a future per-window-teardown change). Same escape as
    /// [`crate::state::input::KeyboardState::set`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn clear(&mut self) {
        self.registrations.clear();
        self.pending.clear();
        self.delivered.clear();
        self.buffered.clear();
        self.next_handle = 0;
        self.devices.clear();
    }
}

/// Whether `device` is a registration that covers the class for `hwnd`.
fn registration_covers(device: &RawInputDevice, hwnd: u64, usage_page: u16, usage: u16) -> bool {
    device.target_window == hwnd
        && device.usage_page == usage_page
        && (device.flags & RIDEV_PAGEONLY != 0 || device.usage == usage)
}

/// Read the 5th Win64 stack argument (`[RSP+0x28]` at handler entry), the slot
/// the `cbSizeHeader` argument of `GetRawInputData` / `GetRawInputBuffer`
/// arrives in.
fn stack_arg5(engine: &mut dyn wie_cpu::CpuEngine) -> Result<u64> {
    let rsp = engine.read_rsp()?;
    let mut bytes = [0_u8; 8];
    engine.mem_read(rsp.wrapping_add(0x28), &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Low 32 bits of a Win64 register as a `u32` argument.
fn arg_u32(value: u64, name: &str) -> Result<u32> {
    u32::try_from(value & u64::from(u32::MAX)).with_context(|| format!("{name} does not fit u32"))
}

/// Read `count` guest `RAWINPUTDEVICE` entries starting at `va`.
fn read_raw_input_devices(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
    count: u32,
) -> Result<Vec<RawInputDevice>> {
    let stride = u64::from(RAW_INPUT_DEVICE_SIZE);
    let mut devices = Vec::with_capacity(count as usize);
    for index in 0..u64::from(count) {
        let entry_va = checked_address(va, index.saturating_mul(stride), "RAWINPUTDEVICE");
        let device = with_typed_read::<RawInputDevice, _, _>(engine, entry_va, |slot| Ok(*slot))
            .context("failed to read RAWINPUTDEVICE")?;
        devices.push(device);
    }
    Ok(devices)
}

/// Write one guest `RAWINPUTDEVICE` at `va`.
fn write_raw_input_device(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
    device: &RawInputDevice,
) -> Result<()> {
    with_typed_write::<RawInputDevice, _, _>(engine, va, |slot| {
        *slot = *device;
        Ok(())
    })
    .context("failed to write RAWINPUTDEVICE")
}

/// Write the synthesized `RIDI_DEVICEINFO` for `device` at `va`.
fn write_rid_device_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
    device: &RawInputDeviceRecord,
) -> Result<()> {
    with_typed_write::<RidDeviceInfo, _, _>(engine, va, |info| {
        info.cb_size = RID_DEVICE_INFO_SIZE;
        info.device_type = device.device_type;
        info.payload = [0; 24];
        Ok(())
    })
    .context("failed to write RID_DEVICE_INFO")?;
    // The union payload goes through the concrete member view at +0x08, which
    // is byte-identical to the C union.
    match device.device_type {
        RIM_TYPE_MOUSE => with_typed_write::<RidDeviceInfoMouse, _, _>(
            engine,
            checked_address(va, 8, "RID_DEVICE_INFO union"),
            |slot| {
                *slot = device.mouse;
                Ok(())
            },
        ),
        RIM_TYPE_KEYBOARD => with_typed_write::<RidDeviceInfoKeyboard, _, _>(
            engine,
            checked_address(va, 8, "RID_DEVICE_INFO union"),
            |slot| {
                *slot = device.keyboard;
                Ok(())
            },
        ),
        _ => with_typed_write::<RidDeviceInfoHid, _, _>(
            engine,
            checked_address(va, 8, "RID_DEVICE_INFO union"),
            |slot| {
                *slot = device.hid;
                Ok(())
            },
        ),
    }
    .context("failed to write RID_DEVICE_INFO union member")
}

/// One packed synthesized record, ready for the host lane to place behind a
/// `WM_INPUT` `lParam`.
///
/// The `WM_INPUT` `lParam` is [`Self::raw_handle`], a fake `HRAWINPUT` — NOT a
/// guest address. That matches Windows, where `HRAWINPUT` (winuser.h:6291) is
/// an opaque system handle a guest passes back to `GetRawInputData` and must
/// never dereference; WIE resolves it against [`RawInputState::delivered`].
/// (`WM_DROPFILES`' `FAKE_HDROP` is the same trick for the same reason.) The
/// host therefore needs none of the `RAWINPUT` layout types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawInputRecordBytes {
    /// The window the record is destined for; `0` means input-sink delivery.
    pub(crate) target: u64,
    /// `RAWINPUTHEADER.dwType` — 0 mouse, 1 keyboard, 2 HID.
    pub(crate) device_type: u32,
    /// `RAWINPUTHEADER.hDevice` — the synthesized device handle.
    pub(crate) device: u64,
    /// `GET_RAWINPUT_CODE_WPARAM(wParam)` — `RIM_INPUT` for a focused window,
    /// `RIM_INPUTSINK` for a background-only one.
    pub(crate) wparam_code: u32,
    /// The fake `HRAWINPUT` to pass as the `WM_INPUT` `lParam`.
    pub raw_handle: u64,
    /// The full packed record: `RAWINPUTHEADER` + payload, unaligned, so the
    /// guest's `NEXTRAWINPUTBLOCK` walk finds exactly what this lane produced.
    pub(crate) bytes: Vec<u8>,
}

impl RawInputRecordBytes {
    /// The `WM_INPUT` message this record rides in (winuser.h:1173).
    #[must_use]
    pub fn message(&self) -> u32 {
        super::WM_INPUT
    }

    /// `RAWINPUTHEADER.dwSize` for the record — its packed byte length.
    #[must_use]
    pub fn record_size(&self) -> u32 {
        u32::try_from(self.bytes.len()).unwrap_or(u32::MAX)
    }

    /// The `WM_INPUT` post for this record, delivered to `hwnd`.
    fn into_post(self, hwnd: u64) -> RawInputPost {
        RawInputPost {
            hwnd,
            wparam: u64::from(self.wparam_code),
            lparam: self.raw_handle,
        }
    }
}

/// One `WM_INPUT` the host must post, already resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawInputPost {
    /// The guest window that receives the message.
    pub hwnd: u64,
    /// `wParam` — `GET_RAWINPUT_CODE_WPARAM`: `RIM_INPUT` for foreground
    /// delivery, `RIM_INPUTSINK` for a `RIDEV_INPUTSINK` window that does not
    /// have focus.
    pub wparam: u64,
    /// `lParam` — the record's fake `HRAWINPUT`.
    pub lparam: u64,
}

/// The `(usUsagePage, usUsage)` of the synthesized keyboard — hidusage.h:36
/// and :56.
const KEYBOARD_CLASS: (u16, u16) = (HID_USAGE_PAGE_GENERIC, HID_USAGE_GENERIC_KEYBOARD);
/// The `(usUsagePage, usUsage)` of the synthesized mouse — hidusage.h:36
/// and :53.
const MOUSE_CLASS: (u16, u16) = (HID_USAGE_PAGE_GENERIC, HID_USAGE_GENERIC_MOUSE);

/// Queue a synthesized keyboard record for `target` (`0` = input sink).
///
/// The host event path should call [`post_raw_keyboard_event`] instead — it
/// applies the registration filter. This is the raw enqueue, for a caller that
/// already decided the target. `make_code` is always 0 from the host (winit
/// reports no scancode), `flags` is `RI_KEY_MAKE` (down) or `RI_KEY_BREAK`
/// (up), and `message` is the `WM_KEYDOWN` / `WM_KEYUP` / `WM_SYS*` value the
/// guest expects inside `RAWKEYBOARD.Message`. `ExtraInformation` is always 0:
/// winit reports none, so a synthesized record never carries device-specific
/// extra data.
pub fn enqueue_raw_keyboard(target: u64, make_code: u16, flags: u16, vkey: u16, message: u32) {
    let keyboard = RawKeyboard {
        make_code,
        flags,
        reserved: 0,
        vkey,
        message,
        extra_information: 0,
    };
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .enqueue_keyboard(target, keyboard);
}

/// Queue a synthesized mouse record for `target` (`0` = input sink).
///
/// The host event path should call [`post_raw_mouse_event`] instead — it
/// applies the registration filter. `mouse_flags` is `MOUSE_MOVE_ABSOLUTE` or
/// `MOUSE_MOVE_RELATIVE`; `button_flags` is the `RI_MOUSE_*` transition bits
/// and `button_data` the wheel delta.
pub fn enqueue_raw_mouse(
    target: u64,
    mouse_flags: u16,
    button_flags: u16,
    button_data: u16,
    raw_buttons: u32,
    last_x: i32,
    last_y: i32,
) {
    let mouse = RawMouse {
        mouse_flags,
        _flags_pad: [0; 2],
        button_flags,
        button_data,
        raw_buttons,
        last_x,
        last_y,
        extra_information: 0,
    };
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .enqueue_mouse(target, mouse);
}

/// Synthesize one keyboard record and return the `WM_INPUT` posts it produces.
///
/// **This is the host `WM_INPUT` lane's entry point** (winit's
/// `KeyboardInput` arm). It applies the whole registration filter, so the
/// caller only hands over the target windows and posts what comes back:
///
/// * `primary` — the window the legacy `WM_KEY*` messages go to (the guest
///   focus window, else the event window's own hwnd). An `RIM_INPUT` post is
///   produced only if that window registered the keyboard class.
/// * `sink_candidate` — the winit window's top-level hwnd, which additionally
///   gets `RIM_INPUTSINK` delivery if it registered the class with
///   `RIDEV_INPUTSINK` and does NOT have focus (winuser.h:6471).
/// * `focus` — the guest focus window, for that `RIDEV_INPUTSINK` test.
///
/// `message` is the `WM_KEYDOWN` / `WM_SYSKEYDOWN` / `WM_KEYUP` /
/// `WM_SYSKEYUP` the host posts alongside. Returns an empty vector when nothing
/// is registered, which is the common case.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn post_raw_keyboard_event(
    primary: u64,
    sink_candidate: Option<u64>,
    focus: Option<u64>,
    vkey: u16,
    pressed: bool,
    message: u32,
) -> Vec<RawInputPost> {
    let keyboard = RawKeyboard {
        make_code: 0,
        flags: if pressed {
            crate::guest_layout::RI_KEY_MAKE
        } else {
            crate::guest_layout::RI_KEY_BREAK
        },
        reserved: 0,
        vkey,
        message,
        extra_information: 0,
    };
    post_event(
        KEYBOARD_CLASS,
        primary,
        sink_candidate,
        focus,
        move |state, target| {
            state.enqueue_keyboard(target, keyboard);
        },
    )
}

/// Which mouse button a synthesized `RAWMOUSE` report describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawMouseButton {
    /// Left button — `RI_MOUSE_LEFT_BUTTON_DOWN` / `…_UP`.
    Left,
    /// Right button — `RI_MOUSE_RIGHT_BUTTON_DOWN` / `…_UP`.
    Right,
    /// Middle button — `RI_MOUSE_MIDDLE_BUTTON_DOWN` / `…_UP`.
    Middle,
}

impl RawMouseButton {
    /// The `RAWMOUSE.usButtonFlags` bit for a press (`down`) or release.
    fn flag(self, down: bool) -> u16 {
        use crate::guest_layout::{
            RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN,
            RI_MOUSE_MIDDLE_BUTTON_UP, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
        };
        match (self, down) {
            (Self::Left, true) => RI_MOUSE_LEFT_BUTTON_DOWN,
            (Self::Left, false) => RI_MOUSE_LEFT_BUTTON_UP,
            (Self::Right, true) => RI_MOUSE_RIGHT_BUTTON_DOWN,
            (Self::Right, false) => RI_MOUSE_RIGHT_BUTTON_UP,
            (Self::Middle, true) => RI_MOUSE_MIDDLE_BUTTON_DOWN,
            (Self::Middle, false) => RI_MOUSE_MIDDLE_BUTTON_UP,
        }
    }
}

/// One host mouse transition, in host terms.
///
/// The host says WHAT happened; this module owns the `RAWMOUSE` encoding, so
/// the GUI event path needs no `RI_MOUSE_*` / `MOUSE_MOVE_*` constants and the
/// flag mapping stays pinned by tests next to the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawMouseReport {
    /// Cursor movement. Encoded as `MOUSE_MOVE_RELATIVE` with the true
    /// per-event delta — the one report WIE can state exactly, because winit
    /// hands it the movement as a position and the previous one is known.
    Movement {
        /// Signed movement since the previous report.
        dx: i32,
        /// Signed movement since the previous report.
        dy: i32,
    },
    /// A button press or release at the client-relative `(x, y)`.
    Button {
        /// Which button.
        button: RawMouseButton,
        /// `true` for a press, `false` for a release.
        down: bool,
        /// Client-relative logical X.
        x: i32,
        /// Client-relative logical Y.
        y: i32,
    },
    /// A wheel notch at the client-relative `(x, y)`.
    Wheel {
        /// `true` for `WM_MOUSEHWHEEL` (horizontal), `false` for `WM_MOUSEWHEEL`.
        horizontal: bool,
        /// Signed whole notches; each becomes a 120-unit `usButtonData` delta.
        notches: i32,
        /// Client-relative logical X.
        x: i32,
        /// Client-relative logical Y.
        y: i32,
    },
}

/// One Win32 wheel notch, in `WHEEL_DELTA` units (winuser.h:1511).
const WHEEL_DELTA: i32 = 120;

impl RawMouseReport {
    /// The `RAWMOUSE` fields this report encodes.
    ///
    /// `usButtonData` is the 16-bit truncation Windows does: a signed delta
    /// masked to 16 bits, which the guest reads back as an `i16`.
    fn encode(self) -> (u16, u16, u16, i32, i32) {
        match self {
            Self::Movement { dx, dy } => (crate::guest_layout::MOUSE_MOVE_RELATIVE, 0, 0, dx, dy),
            Self::Button { button, down, x, y } => (
                crate::guest_layout::MOUSE_MOVE_ABSOLUTE,
                button.flag(down),
                0,
                x,
                y,
            ),
            Self::Wheel {
                horizontal,
                notches,
                x,
                y,
            } => (
                crate::guest_layout::MOUSE_MOVE_ABSOLUTE,
                if horizontal {
                    crate::guest_layout::RI_MOUSE_HWHEEL
                } else {
                    crate::guest_layout::RI_MOUSE_WHEEL
                },
                wheel_delta(notches),
                x,
                y,
            ),
        }
    }
}

/// `notches` × 120 truncated to the 16 bits `RAWMOUSE.usButtonData` holds.
fn wheel_delta(notches: i32) -> u16 {
    let scaled = notches.saturating_mul(WHEEL_DELTA);
    u16::try_from(scaled & 0xFFFF).unwrap_or(0)
}

/// Synthesize one mouse record and return the `WM_INPUT` posts it produces.
///
/// **This is the host `WM_INPUT` lane's entry point** for `CursorMoved`,
/// `MouseInput`, and `MouseWheel`. The window arguments mirror
/// [`post_raw_keyboard_event`]. `raw_buttons` is the host's current button
/// bitmask, recorded in `RAWMOUSE.ulRawButtons` (the legacy `MK_*` mask, which
/// is bit-compatible with what a guest expects there).
#[must_use]
pub fn post_raw_mouse_event(
    primary: u64,
    sink_candidate: Option<u64>,
    focus: Option<u64>,
    report: RawMouseReport,
    raw_buttons: u32,
) -> Vec<RawInputPost> {
    let (mouse_flags, button_flags, button_data, last_x, last_y) = report.encode();
    let mouse = RawMouse {
        mouse_flags,
        _flags_pad: [0; 2],
        button_flags,
        button_data,
        raw_buttons,
        last_x,
        last_y,
        extra_information: 0,
    };
    post_event(
        MOUSE_CLASS,
        primary,
        sink_candidate,
        focus,
        move |state, target| {
            state.enqueue_mouse(target, mouse);
        },
    )
}

/// The shared delivery policy: filter by registration, queue, publish, and
/// return one [`RawInputPost`] per delivered record.
///
/// One lock acquisition for the whole event — the host thread must never take
/// the big `WinApiState` lock, and this module's `Mutex` is not reentrant, so
/// every state read and write for one event happens here. The enqueue closure
/// only touches the state, never the engine, so the lock is never held across
/// a guest-memory access.
fn post_event(
    class: (u16, u16),
    primary: u64,
    sink_candidate: Option<u64>,
    focus: Option<u64>,
    enqueue: impl Fn(&mut RawInputState, u64),
) -> Vec<RawInputPost> {
    let (usage_page, usage) = class;
    let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
    let mut posts = Vec::new();

    // Foreground delivery. A `RIDEV_INPUTSINK` registration must NOT receive
    // input while its window holds focus (winuser.h:6471), so a focused
    // input-sink window falls through to the sink pass below.
    let sink_only = state.is_input_sink(primary, usage_page, usage) && focus == Some(primary);
    if primary != 0 && state.is_registered(primary, usage_page, usage) && !sink_only {
        enqueue(&mut state, primary);
        for record in state.take_pending_for_window(primary) {
            posts.push(state.publish(&record).into_post(primary));
        }
    }

    // Background-only delivery to the event window's own top level.
    if let Some(sink) = sink_candidate
        && sink != 0
        && sink != primary
        && focus != Some(sink)
        && state.is_registered(sink, usage_page, usage)
        && state.is_input_sink(sink, usage_page, usage)
    {
        // Target 0 is the input-sink queue, so the record codes itself
        // `RIM_INPUTSINK` (see `PendingRawInput::wparam_code`).
        enqueue(&mut state, 0);
        for record in state.take_pending_for_window(0) {
            posts.push(state.publish(&record).into_post(sink));
        }
    }
    posts
}

/// Remove and return the packed records destined for `hwnd`.
///
/// The host path uses [`post_raw_keyboard_event`] / [`post_raw_mouse_event`];
/// this is the lower-level drain, for a caller that queued records itself (and
/// for the tests that pin the record contract). Every record it returns is
/// published, so its `raw_handle` resolves through `GetRawInputData`. An empty
/// vector means there is nothing to post.
#[must_use]
pub fn drain_raw_input_for_window(hwnd: u64) -> Vec<RawInputRecordBytes> {
    let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
    let records = state.take_pending_for_window(hwnd);
    records.iter().map(|record| state.publish(record)).collect()
}

/// Register (or unregister) one raw-input device class for `hwnd`.
///
/// The guest-facing path is `RegisterRawInputDevices`, which is what a real
/// guest uses; this is the same state transition for a host-side caller that
/// must register a class without a guest call (today: the host lane's tests).
/// `flags` is the `RIDEV_*` mask — `RIDEV_REMOVE` unregisters.
pub fn register_raw_input_class(hwnd: u64, usage_page: u16, usage: u16, flags: u32) {
    let device = RawInputDevice {
        usage_page,
        usage,
        flags,
        target_window: hwnd,
    };
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(device);
}

/// Whether `hwnd` registered the keyboard class, so the host lane knows
/// whether to post `WM_INPUT` for a key at all.
#[must_use]
pub fn is_raw_keyboard_registered(hwnd: u64) -> bool {
    is_raw_input_registered(hwnd, KEYBOARD_CLASS.0, KEYBOARD_CLASS.1)
}

/// Whether `hwnd` registered the mouse class.
#[must_use]
pub fn is_raw_mouse_registered(hwnd: u64) -> bool {
    is_raw_input_registered(hwnd, MOUSE_CLASS.0, MOUSE_CLASS.1)
}

/// Whether `hwnd` set `RIDEV_EXCLUDE` for the keyboard class, i.e. asked for
/// the legacy `WM_KEY*` messages (and the `WM_CHAR`s translated from them) to
/// be suppressed in favour of `WM_INPUT`.
#[must_use]
pub fn is_legacy_keyboard_input_excluded(hwnd: u64) -> bool {
    is_raw_input_excluded(hwnd, KEYBOARD_CLASS.0, KEYBOARD_CLASS.1)
}

/// Whether `hwnd` set `RIDEV_EXCLUDE` for the mouse class, i.e. asked for the
/// legacy `WM_MOUSE*` messages to be suppressed in favour of `WM_INPUT`.
#[must_use]
pub fn is_legacy_mouse_input_excluded(hwnd: u64) -> bool {
    is_raw_input_excluded(hwnd, MOUSE_CLASS.0, MOUSE_CLASS.1)
}

/// Whether `hwnd` registered the `(usage_page, usage)` raw-input class, so the
/// host lane knows whether to post `WM_INPUT` at all.
#[must_use]
pub fn is_raw_input_registered(hwnd: u64, usage_page: u16, usage: u16) -> bool {
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_registered(hwnd, usage_page, usage)
}

/// Whether `hwnd` set `RIDEV_EXCLUDE` for the class, i.e. asked for the legacy
/// `WM_MOUSE*` / `WM_KEY*` messages to be suppressed for it.
#[must_use]
pub fn is_raw_input_excluded(hwnd: u64, usage_page: u16, usage: u16) -> bool {
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_excluded(hwnd, usage_page, usage)
}

/// Whether `hwnd` set `RIDEV_INPUTSINK` for the class, i.e. asked to receive
/// raw input only while it does NOT have focus.
#[must_use]
pub fn is_raw_input_input_sink(hwnd: u64, usage_page: u16, usage: u16) -> bool {
    raw_input_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_input_sink(hwnd, usage_page, usage)
}

/// Dispatch the RawInput lane of `dispatch_user32_extra`.
pub(crate) fn dispatch_raw_input(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "registerrawinputdevices" => Ok(Some(handle_register_raw_input_devices(ctx)?)),
        "getregisteredrawinputdevices" => Ok(Some(handle_get_registered_raw_input_devices(ctx)?)),
        "getrawinputdevicelist" => Ok(Some(handle_get_raw_input_device_list(ctx)?)),
        "getrawinputdeviceinfoa" => Ok(Some(handle_get_raw_input_device_info(ctx, false)?)),
        "getrawinputdeviceinfow" => Ok(Some(handle_get_raw_input_device_info(ctx, true)?)),
        "getrawinputbuffer" => Ok(Some(handle_get_raw_input_buffer(ctx)?)),
        "getrawinputdata" => Ok(Some(handle_get_raw_input_data(ctx)?)),
        "defrawinputproc" => Ok(Some(handle_def_raw_input_proc(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `USER32.dll!RegisterRawInputDevices`.
///
/// Win64 ABI: `rcx` = `pRawInputDevices`, `rdx` = `uiNumDevices`, `r8` =
/// `cbSize`.
///
/// `uiNumDevices == 0` is a successful no-op (that is what the removed
/// "always FALSE" stub got wrong, and why every guest that probes RawInput
/// degraded). A NULL array with a non-zero count, or a `cbSize` that is not
/// `sizeof(RAWINPUTDEVICE)`, is `ERROR_INVALID_PARAMETER` + FALSE.
pub fn handle_register_raw_input_devices(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let devices_va = read_arg(engine, ArgReg::Rcx, "RegisterRawInputDevices")?;
    let count_raw = read_arg(engine, ArgReg::Rdx, "RegisterRawInputDevices")?;
    let cb_size = read_arg(engine, ArgReg::R8, "RegisterRawInputDevices")?;
    let count = arg_u32(count_raw, "uiNumDevices")?;

    if count == 0 {
        return ctx.finish(1);
    }
    if devices_va == 0 || cb_size != u64::from(RAW_INPUT_DEVICE_SIZE) {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    let devices = read_raw_input_devices(engine, devices_va, count)?;
    {
        let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        for device in devices {
            state.register(device);
        }
    }
    ctx.finish(1)
}

/// Handles `USER32.dll!GetRegisteredRawInputDevices`.
///
/// Win64 ABI: `rcx` = `pRawInputDevices`, `rdx` = `puiNumDevices`, `r8` =
/// `cbSize`.
///
/// Follows the Windows two-call protocol: the count slot is always written,
/// and a capacity below the registration count copies nothing and returns 0.
/// This export did not exist in WIE at all before this lane.
pub fn handle_get_registered_raw_input_devices(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let list_va = read_arg(engine, ArgReg::Rcx, "GetRegisteredRawInputDevices")?;
    let count_va = read_arg(engine, ArgReg::Rdx, "GetRegisteredRawInputDevices")?;
    let cb_size = read_arg(engine, ArgReg::R8, "GetRegisteredRawInputDevices")?;

    if count_va == 0 || cb_size != u64::from(RAW_INPUT_DEVICE_SIZE) {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    let capacity =
        read_u32(engine, count_va).context("failed to read GetRegisteredRawInputDevices count")?;
    let registrations = {
        let state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.registrations().to_vec()
    };
    let needed = u32::try_from(registrations.len()).unwrap_or(u32::MAX);
    write_guest_u32(engine, count_va, needed)
        .context("failed to write GetRegisteredRawInputDevices count")?;

    if list_va == 0 || needed > capacity {
        return ctx.finish(0);
    }
    let stride = u64::from(RAW_INPUT_DEVICE_SIZE);
    for (index, registration) in registrations.iter().enumerate() {
        let index = u64::try_from(index).unwrap_or(0);
        let entry_va = checked_address(list_va, index.saturating_mul(stride), "RAWINPUTDEVICE");
        write_raw_input_device(engine, entry_va, &registration.device)?;
    }
    ctx.finish(u64::from(needed))
}

/// Handles `USER32.dll!GetRawInputDeviceList`.
///
/// Win64 ABI: `rcx` = `pRawInputDeviceList`, `rdx` = `puiNumDevices`, `r8` =
/// `cbSize`.
///
/// A NULL `puiNumDevices` is `ERROR_INVALID_PARAMETER`. A NULL list (or a
/// capacity below the device count) writes the count and returns 0, so a
/// guest's two-call sizing loop terminates.
pub fn handle_get_raw_input_device_list(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let list_va = read_arg(engine, ArgReg::Rcx, "GetRawInputDeviceList")?;
    let count_va = read_arg(engine, ArgReg::Rdx, "GetRawInputDeviceList")?;
    let cb_size = read_arg(engine, ArgReg::R8, "GetRawInputDeviceList")?;

    if count_va == 0 || cb_size != u64::from(RAW_INPUT_DEVICE_LIST_SIZE) {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let capacity =
        read_u32(engine, count_va).context("failed to read GetRawInputDeviceList count")?;

    let devices = {
        let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.devices().to_vec()
    };
    let needed = u32::try_from(devices.len()).unwrap_or(u32::MAX);
    write_guest_u32(engine, count_va, needed)
        .context("failed to write GetRawInputDeviceList count")?;

    if list_va == 0 || needed > capacity {
        return ctx.finish(0);
    }
    let stride = u64::from(RAW_INPUT_DEVICE_LIST_SIZE);
    for (index, device) in devices.iter().enumerate() {
        let index = u64::try_from(index).unwrap_or(0);
        let entry_va = checked_address(list_va, index.saturating_mul(stride), "RAWINPUTDEVICELIST");
        with_typed_write::<RawInputDeviceList, _, _>(engine, entry_va, |slot| {
            slot.device = device.handle;
            slot.device_type = device.device_type;
            Ok(())
        })
        .context("failed to write RAWINPUTDEVICELIST")?;
    }
    ctx.finish(u64::from(needed))
}

/// Handles `USER32.dll!GetRawInputDeviceInfoA` and `...W`.
///
/// Win64 ABI: `rcx` = `hDevice`, `rdx` = `uiCommand`, `r8` = `pData`, `r9` =
/// `pcbSize`. `wide` selects the UTF-16 path.
///
/// `pcbSize` is in/out in characters: with a NULL `pData` (or a zero slot) the
/// function returns 0 and stores the required size. A capacity below the
/// required size is `ERROR_INSUFFICIENT_BUFFER` + 0. On success the return is
/// the number of characters copied (the NUL excluded) for `RIDI_DEVICENAME`,
/// and the byte count for `RIDI_DEVICEINFO`.
pub fn handle_get_raw_input_device_info(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let api_name = if wide {
        "GetRawInputDeviceInfoW"
    } else {
        "GetRawInputDeviceInfoA"
    };
    let engine = &mut *ctx.engine;
    let device_handle = read_arg(engine, ArgReg::Rcx, api_name)?;
    let command = arg_u32(read_arg(engine, ArgReg::Rdx, api_name)?, "uiCommand")?;
    let data_va = read_arg(engine, ArgReg::R8, api_name)?;
    let size_va = read_arg(engine, ArgReg::R9, api_name)?;

    if size_va == 0 {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let capacity =
        read_u32(engine, size_va).with_context(|| format!("failed to read {api_name} size"))?;

    let device = {
        let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.device(device_handle)
    };
    let Some(device) = device else {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    };

    match command {
        RIDI_DEVICENAME => {
            let units = device.name.encode_utf16().count();
            let needed = u32::try_from(units.saturating_add(1)).unwrap_or(u32::MAX);
            if data_va == 0 || capacity == 0 {
                write_guest_u32(engine, size_va, needed)
                    .with_context(|| format!("failed to write {api_name} size"))?;
                return ctx.finish(0);
            }
            if capacity < needed {
                write_guest_u32(engine, size_va, needed)
                    .with_context(|| format!("failed to write {api_name} size"))?;
                ctx.state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
                return ctx.finish(0);
            }
            let written = if wide {
                write_guest_utf16_c_string(
                    engine,
                    data_va,
                    usize::try_from(needed).unwrap_or(DEVICE_NAME_MAX),
                    &device.name,
                )
            } else {
                write_guest_ansi_c_string(
                    engine,
                    data_va,
                    usize::try_from(needed).unwrap_or(DEVICE_NAME_MAX),
                    &device.name,
                )
            }
            .with_context(|| format!("failed to write the {api_name} device path"))?;
            write_guest_u32(engine, size_va, needed)
                .with_context(|| format!("failed to write {api_name} size"))?;
            ctx.finish(u64::try_from(written).unwrap_or(0))
        }
        RIDI_DEVICEINFO => {
            if data_va == 0 || capacity == 0 {
                write_guest_u32(engine, size_va, RID_DEVICE_INFO_SIZE)
                    .with_context(|| format!("failed to write {api_name} size"))?;
                return ctx.finish(0);
            }
            if capacity < RID_DEVICE_INFO_SIZE {
                write_guest_u32(engine, size_va, RID_DEVICE_INFO_SIZE)
                    .with_context(|| format!("failed to write {api_name} size"))?;
                ctx.state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
                return ctx.finish(0);
            }
            write_rid_device_info(engine, data_va, &device)?;
            write_guest_u32(engine, size_va, RID_DEVICE_INFO_SIZE)
                .with_context(|| format!("failed to write {api_name} size"))?;
            ctx.finish(u64::from(RID_DEVICE_INFO_SIZE))
        }
        // RIDI_PREPARSEDDATA (and anything else) needs a real HID report
        // table, which WIE does not have.
        _ => {
            ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
            ctx.finish(0)
        }
    }
}

/// Handles `USER32.dll!GetRawInputBuffer`.
///
/// Win64 ABI: `rcx` = `pData`, `rdx` = `pcbSize`, and `cbSizeHeader` on the
/// stack as the 3rd argument.
///
/// It serialises the records the system buffered for the guest — NOT the
/// transient post-time queue, which the host delivery path drains as it posts
/// each `WM_INPUT`. The source is [`RawInputState::pending`] plus
/// [`RawInputState::buffered`], i.e. every record WIE synthesized, whether or
/// not its `WM_INPUT` has been posted yet (see the module's "Two views of ONE
/// buffered input" note). It does not consult [`RawInputState::delivered`]:
/// that store belongs to the `GetRawInputData(lParam)` view, and a drain here
/// must not invalidate an `HRAWINPUT` the guest is still holding.
///
/// A NULL `pData` query and an under-sized buffer are both **non-consuming** —
/// the records stay queued so the guest can retry with a real buffer — and both
/// return 0 after writing the required size. A successful fill consumes the
/// records it copied.
///
/// Records are packed exactly as Windows packs them: each is a
/// `RAWINPUTHEADER` plus its payload, with no per-record alignment padding, and
/// each `header.dwSize` is that record's own packed size so a guest can walk
/// the chain with `NEXTRAWINPUTBLOCK` (winuser.h:6403). The returned buffer
/// size is the sum of the packed record sizes. The return value is the record
/// count, or 0 when the call was a size query or the capacity was too small.
pub fn handle_get_raw_input_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let data_va = read_arg(engine, ArgReg::Rcx, "GetRawInputBuffer")?;
    let size_va = read_arg(engine, ArgReg::Rdx, "GetRawInputBuffer")?;
    let header_size = arg_u32(stack_arg5(engine)?, "cbSizeHeader")?;

    if size_va == 0 || header_size != RAW_INPUT_HEADER_SIZE {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let capacity =
        read_u32(engine, size_va).context("failed to read the GetRawInputBuffer size")?;

    // Size first, WITHOUT consuming: a size query or an under-sized buffer must
    // leave the records queued so the guest can retry with a real buffer.
    let needed = {
        let state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.buffered_size()
    };
    write_guest_u32(engine, size_va, needed)
        .context("failed to write the GetRawInputBuffer size")?;

    if data_va == 0 || capacity == 0 || capacity < needed {
        return ctx.finish(0);
    }

    let records = {
        let mut state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.take_all_buffered()
    };
    let count = u32::try_from(records.len()).unwrap_or(u32::MAX);

    let mut packed = Vec::new();
    for record in &records {
        record.append_packed(&mut packed);
    }
    write_guest_bytes(engine, data_va, &packed)
        .context("failed to write the GetRawInputBuffer records")?;
    ctx.finish(u64::from(count))
}

/// Handles `USER32.dll!GetRawInputData`.
///
/// Win64 ABI: `rcx` = `hRawInput`, `rdx` = `uiCommand`, `r8` = `pData`, `r9` =
/// `pcbSize`, and `cbSizeHeader` on the stack as the 5th argument.
///
/// `hRawInput` resolves two ways, because a guest passes two different things
/// to this API:
///
/// 1. The `WM_INPUT` `lParam` — a fake `HRAWINPUT` this lane handed out
///    ([`RawInputState::delivered`]). Windows' `HRAWINPUT` is an opaque system
///    handle, never a guest-dereferenceable pointer, so WIE's is too.
/// 2. A slot inside a `GetRawInputBuffer` fill — a real guest address, copied
///    straight out of guest memory, which is what that path wrote there.
///
/// It resolves neither store the `GetRawInputBuffer` view uses: this is the
/// `HRAWINPUT` view, so a lookup goes to [`RawInputState::delivered`] and, for
/// a guest pointer, to guest memory. Notably a `GetRawInputBuffer` drain does
/// not invalidate a handle — the two views have independent lifetimes (see the
/// module's "Two views of ONE buffered input" note).
///
/// `pcbSize` is in/out: with a NULL `pData` (or a zero slot) the function
/// returns 0 and stores the required size. A capacity below the required size
/// is `ERROR_INVALID_PARAMETER` + `(UINT)-1`. On success the function copies
/// the record, stores its size, and returns it.
///
/// `RID_HEADER` copies only the `RAWINPUTHEADER`; `RID_INPUT` copies the whole
/// record, whose size is the `header.dwSize` the record already carries.
pub fn handle_get_raw_input_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let raw_va = read_arg(engine, ArgReg::Rcx, "GetRawInputData")?;
    let command = arg_u32(
        read_arg(engine, ArgReg::Rdx, "GetRawInputData")?,
        "uiCommand",
    )?;
    let data_va = read_arg(engine, ArgReg::R8, "GetRawInputData")?;
    let size_va = read_arg(engine, ArgReg::R9, "GetRawInputData")?;
    let header_size = arg_u32(stack_arg5(engine)?, "cbSizeHeader")?;

    if raw_va == 0 || size_va == 0 || header_size != RAW_INPUT_HEADER_SIZE {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(u64::from(u32::MAX));
    }
    let capacity = read_u32(engine, size_va).context("failed to read the GetRawInputData size")?;

    let reject = |ctx: &mut HandlerContext<'_>| -> Result<WinApiHandlerResult> {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        ctx.finish(u64::from(u32::MAX))
    };

    // A delivered `HRAWINPUT` first: the record is in this lane's own store,
    // so the guest's `lParam` resolves without a guest-memory round trip. The
    // copy is ≤ 48 bytes, so cloning it out of the state is cheaper than
    // holding the (non-reentrant) lock across engine reads.
    let delivered = {
        let state = raw_input_state().lock().unwrap_or_else(|e| e.into_inner());
        state.delivered(raw_va).cloned()
    };

    let Ok(header) = delivered.as_ref().map_or_else(
        || with_typed_read::<RawInputHeader, _, _>(engine, raw_va, |slot| Ok(*slot)),
        |record| Ok(record.header),
    ) else {
        return reject(ctx);
    };
    // Only the two record types WIE synthesizes are valid, and dwSize must
    // cover the header plus that type's payload.
    let minimum = match header.device_type {
        RIM_TYPE_MOUSE => RAW_INPUT_HEADER_SIZE + RAW_MOUSE_SIZE,
        RIM_TYPE_KEYBOARD => RAW_INPUT_HEADER_SIZE + RAW_KEYBOARD_SIZE,
        _ => return reject(ctx),
    };
    if header.size < minimum {
        return reject(ctx);
    }

    let needed = match command {
        RID_HEADER => RAW_INPUT_HEADER_SIZE,
        RID_INPUT => header.size,
        _ => return reject(ctx),
    };

    if data_va == 0 || capacity == 0 {
        write_guest_u32(engine, size_va, needed)
            .context("failed to write the GetRawInputData size")?;
        return ctx.finish(0);
    }
    if capacity < needed {
        ctx.state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(u64::from(u32::MAX));
    }

    let copy = usize::try_from(needed).unwrap_or(0);
    let bytes = match delivered {
        Some(record) => record
            .bytes
            .get(..copy)
            .map_or_else(Vec::new, <[u8]>::to_vec),
        None => {
            let mut bytes = vec![0_u8; copy];
            read_guest_bytes(engine, raw_va, &mut bytes)
                .context("failed to read the GetRawInputData record")?;
            bytes
        }
    };
    write_guest_bytes(engine, data_va, &bytes)
        .context("failed to write the GetRawInputData record")?;
    write_guest_u32(engine, size_va, needed).context("failed to write the GetRawInputData size")?;
    ctx.finish(u64::from(needed))
}

/// Handles `USER32.dll!DefRawInputProc`.
///
/// Win64 ABI: `rcx` = `paRawInput` (a `PRAWINPUT*` array), `rdx` = `nInput`,
/// `r8` = `cbSizeHeader`.
///
/// With a NULL array or a non-positive count there is nothing to forward, so
/// the call completes with a zero `LRESULT`. Otherwise the FIRST record is
/// forwarded to the guest `RAWINPUTPROC` through the one-shot
/// [`WinApiControlSignal::GuestCallbackRequested`] bridge — the same
/// limitation `EnumWindows` (user32/enum_caret.rs) has. The `LRESULT` the guest
/// proc returns is not propagated back by that bridge, so the API always
/// completes as 0.
pub fn handle_def_raw_input_proc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let list_va = read_arg(engine, ArgReg::Rcx, "DefRawInputProc")?;
    let count_raw = read_arg(engine, ArgReg::Rdx, "DefRawInputProc")?;
    let header_size = arg_u32(
        read_arg(engine, ArgReg::R8, "DefRawInputProc")?,
        "cbSizeHeader",
    )?;
    let count = arg_u32(count_raw, "nInput")?;

    if list_va == 0 || count == 0 || header_size != RAW_INPUT_HEADER_SIZE {
        return ctx.finish(0);
    }

    // The first `PRAWINPUT` slot points at the record to forward.
    let mut slot = [0_u8; 8];
    engine
        .mem_read(list_va, &mut slot)
        .context("failed to read the DefRawInputProc record array")?;
    let record_va = u64::from_le_bytes(slot);
    if record_va == 0 {
        return ctx.finish(0);
    }

    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: list_va,
            window_handle: record_va,
            message: super::WM_INPUT,
            word_parameter: 0,
            long_parameter: 0,
            unicode: false,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}
