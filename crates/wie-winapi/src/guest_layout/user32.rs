//! user32 lane: `WndClassEx`, `WndClass`, `CreateStruct`, `WinRect`,
//! `WinPoint`, `MenuItemInfo`, `TrackMouseEvent`, and the RawInput family
//! (`RawInputHeader`, `RawMouse`, `RawKeyboard`, `RawInput`,
//! `RawInputDevice`, `RawInputDeviceList`, `RidDeviceInfo`).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// --- user32 lane: WNDCLASSEX/WNDCLASS, CREATESTRUCT, RECT/POINT, MENUITEMINFO, TRACKMOUSEEVENT ---

/// Win64 `WNDCLASSEXW`/`WNDCLASSEXA` (winuser.h): `UINT cbSize` @0x00,
/// `UINT style` @0x04, `WNDPROC lpfnWndProc` @0x08, `INT cbClsExtra` @0x10,
/// `INT cbWndExtra` @0x14, `HINSTANCE hInstance` @0x18, `HICON hIcon` @0x20,
/// `HCURSOR hCursor` @0x28, `HBRUSH hbrBackground` @0x30, `LPCWSTR
/// lpszMenuName` @0x38, `LPCWSTR lpszClassName` @0x40, `HICON hIconSm` @0x48 —
/// 80 bytes, align 8.
///
/// The A and W variants share this layout; only the pointed-to strings
/// differ, so one struct serves both `RegisterClassExA` and `RegisterClassExW`.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WndClassEx {
    pub(crate) cb_size: u32,
    pub(crate) style: u32,
    pub(crate) window_proc: u64,
    pub(crate) cb_cls_extra: i32,
    pub(crate) cb_wnd_extra: i32,
    pub(crate) instance_handle: u64,
    pub(crate) icon_handle: u64,
    pub(crate) cursor_handle: u64,
    pub(crate) background_brush: u64,
    pub(crate) menu_name: u64,
    pub(crate) class_name_ptr: u64,
    pub(crate) small_icon_handle: u64,
}

/// Compile-time layout check for [`WndClassEx`]: the offsets mirror the
/// constants the per-field `RegisterClassEx*` handlers pinned (including the
/// +0x38 `lpszMenuName` this repo's class-menu history hinges on).
const _: () = {
    assert!(
        core::mem::size_of::<WndClassEx>() == 80,
        "WNDCLASSEX must be 80 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_size) == 0x00,
        "cbSize @0x00"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, style) == 0x04,
        "style @0x04"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, window_proc) == 0x08,
        "lpfnWndProc @0x08"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_cls_extra) == 0x10,
        "cbClsExtra @0x10"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_wnd_extra) == 0x14,
        "cbWndExtra @0x14"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, instance_handle) == 0x18,
        "hInstance @0x18"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, icon_handle) == 0x20,
        "hIcon @0x20"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cursor_handle) == 0x28,
        "hCursor @0x28"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, background_brush) == 0x30,
        "hbrBackground @0x30"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, menu_name) == 0x38,
        "lpszMenuName @0x38"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, class_name_ptr) == 0x40,
        "lpszClassName @0x40"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, small_icon_handle) == 0x48,
        "hIconSm @0x48"
    );
};

/// Win64 `WNDCLASSW`/`WNDCLASSA` (winuser.h) — the non-Ex variant (no
/// `cbSize`, no `hIconSm`): `UINT style` @0x00, [pad] @0x04, `WNDPROC
/// lpfnWndProc` @0x08, `INT cbClsExtra` @0x10, `INT cbWndExtra` @0x14,
/// `HINSTANCE hInstance` @0x18, `HICON hIcon` @0x20, `HCURSOR hCursor` @0x28,
/// `HBRUSH hbrBackground` @0x30, `LPCWSTR lpszMenuName` @0x38, `LPCWSTR
/// lpszClassName` @0x40 — 72 bytes, align 8.
///
/// (The 40-byte size sometimes quoted for WNDCLASS is the Win32 layout —
/// 4-byte pointers; the assert table below pins the Win64 72-byte layout the
/// handlers have always read.)
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WndClass {
    pub(crate) style: u32,
    /// Win64 alignment padding between `style` and the `lpfnWndProc` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) window_proc: u64,
    pub(crate) cb_cls_extra: i32,
    pub(crate) cb_wnd_extra: i32,
    pub(crate) instance_handle: u64,
    pub(crate) icon_handle: u64,
    pub(crate) cursor_handle: u64,
    pub(crate) background_brush: u64,
    pub(crate) menu_name: u64,
    pub(crate) class_name_ptr: u64,
}

/// Compile-time layout check for [`WndClass`].
const _: () = {
    assert!(
        core::mem::size_of::<WndClass>() == 72,
        "WNDCLASS must be 72 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WndClass, style) == 0x00,
        "style @0x00"
    );
    assert!(
        core::mem::offset_of!(WndClass, window_proc) == 0x08,
        "lpfnWndProc @0x08"
    );
    assert!(
        core::mem::offset_of!(WndClass, cb_cls_extra) == 0x10,
        "cbClsExtra @0x10"
    );
    assert!(
        core::mem::offset_of!(WndClass, cb_wnd_extra) == 0x14,
        "cbWndExtra @0x14"
    );
    assert!(
        core::mem::offset_of!(WndClass, instance_handle) == 0x18,
        "hInstance @0x18"
    );
    assert!(
        core::mem::offset_of!(WndClass, icon_handle) == 0x20,
        "hIcon @0x20"
    );
    assert!(
        core::mem::offset_of!(WndClass, cursor_handle) == 0x28,
        "hCursor @0x28"
    );
    assert!(
        core::mem::offset_of!(WndClass, background_brush) == 0x30,
        "hbrBackground @0x30"
    );
    assert!(
        core::mem::offset_of!(WndClass, menu_name) == 0x38,
        "lpszMenuName @0x38"
    );
    assert!(
        core::mem::offset_of!(WndClass, class_name_ptr) == 0x40,
        "lpszClassName @0x40"
    );
};

/// Win64 `CREATESTRUCTW`/`CREATESTRUCTA` (winuser.h), the `WM_CREATE` lParam:
/// `LPVOID lpCreateParams` @0x00, `HINSTANCE hInstance` @0x08, `HMENU hMenu`
/// @0x10, `HWND hwndParent` @0x18, `int cy` @0x20, `int cx` @0x24, `int y`
/// @0x28, `int x` @0x2C, `LONG style` @0x30, [pad] @0x34, `LPCWSTR lpszName`
/// @0x38, `LPCWSTR lpszClass` @0x40, `DWORD dwExStyle` @0x48, [pad] @0x4C —
/// 80 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct CreateStruct {
    pub(crate) create_params: u64,
    pub(crate) instance_handle: u64,
    pub(crate) menu_handle: u64,
    pub(crate) parent_handle: u64,
    pub(crate) cy: i32,
    pub(crate) cx: i32,
    pub(crate) y: i32,
    pub(crate) x: i32,
    pub(crate) style: u32,
    /// Win64 alignment padding between `style` and the `lpszName` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) name_ptr: u64,
    pub(crate) class_ptr: u64,
    pub(crate) extended_style: u32,
    /// Trailing alignment padding (76 payload bytes → 80).
    pub(crate) _pad_end: [u8; 4],
}

/// Compile-time layout check for [`CreateStruct`].
const _: () = {
    assert!(
        core::mem::size_of::<CreateStruct>() == 0x50,
        "CREATESTRUCT must be 0x50 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, create_params) == 0x00,
        "lpCreateParams @0x00"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, instance_handle) == 0x08,
        "hInstance @0x08"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, menu_handle) == 0x10,
        "hMenu @0x10"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, parent_handle) == 0x18,
        "hwndParent @0x18"
    );
    assert!(core::mem::offset_of!(CreateStruct, cy) == 0x20, "cy @0x20");
    assert!(core::mem::offset_of!(CreateStruct, cx) == 0x24, "cx @0x24");
    assert!(core::mem::offset_of!(CreateStruct, y) == 0x28, "y @0x28");
    assert!(core::mem::offset_of!(CreateStruct, x) == 0x2C, "x @0x2C");
    assert!(
        core::mem::offset_of!(CreateStruct, style) == 0x30,
        "style @0x30"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, name_ptr) == 0x38,
        "lpszName @0x38"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, class_ptr) == 0x40,
        "lpszClass @0x40"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, extended_style) == 0x48,
        "dwExStyle @0x48"
    );
};

/// Win64 `RECT` (windef.h): `LONG left` @0x00, `LONG top` @0x04, `LONG right`
/// @0x08, `LONG bottom` @0x0C — 16 bytes, align 4. Named `WinRect` (not
/// `Rect`) so it cannot collide with the gdi32 lane's `Rect`.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WinRect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

/// Compile-time layout check for [`WinRect`].
const _: () = {
    assert!(
        core::mem::size_of::<WinRect>() == 16,
        "RECT must be 16 bytes on Win64"
    );
    assert!(core::mem::offset_of!(WinRect, left) == 0, "left @0");
    assert!(core::mem::offset_of!(WinRect, top) == 4, "top @4");
    assert!(core::mem::offset_of!(WinRect, right) == 8, "right @8");
    assert!(core::mem::offset_of!(WinRect, bottom) == 12, "bottom @12");
};

/// Win64 `POINT` (windef.h): `LONG x` @0x00, `LONG y` @0x04 — 8 bytes,
/// align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WinPoint {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

/// Compile-time layout check for [`WinPoint`].
const _: () = {
    assert!(
        core::mem::size_of::<WinPoint>() == 8,
        "POINT must be 8 bytes on Win64"
    );
    assert!(core::mem::offset_of!(WinPoint, x) == 0, "x @0");
    assert!(core::mem::offset_of!(WinPoint, y) == 4, "y @4");
};

/// Win64 `MENUITEMINFO` (winuser.h): `UINT cbSize` @0x00, `UINT fMask` @0x04,
/// `UINT fType` @0x08, `UINT fState` @0x0C, `UINT wID` @0x10, [pad] @0x14,
/// `HMENU hSubMenu` @0x18, `HBITMAP hbmpChecked` @0x20, `HBITMAP
/// hbmpUnchecked` @0x28, `ULONG_PTR dwItemData` @0x30, `LPTSTR dwTypeData`
/// @0x38, `UINT cch` @0x40, [pad] @0x44, `HBITMAP hbmpItem` @0x48 — 80 bytes,
/// align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct MenuItemInfo {
    pub(crate) cb_size: u32,
    pub(crate) f_mask: u32,
    pub(crate) f_type: u32,
    pub(crate) f_state: u32,
    pub(crate) w_id: u32,
    /// Win64 alignment padding between `wID` and the `hSubMenu` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) submenu_handle: u64,
    pub(crate) checked_bitmap: u64,
    pub(crate) unchecked_bitmap: u64,
    pub(crate) item_data: u64,
    pub(crate) type_data_ptr: u64,
    pub(crate) cch: u32,
    /// Win64 alignment padding between `cch` and the `hbmpItem` pointer.
    pub(crate) _pad_after_cch: [u8; 4],
    pub(crate) item_bitmap: u64,
}

/// Compile-time layout check for [`MenuItemInfo`].
const _: () = {
    assert!(
        core::mem::size_of::<MenuItemInfo>() == 80,
        "MENUITEMINFO must be 80 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(MenuItemInfo, cb_size) == 0,
        "cbSize @0"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, f_mask) == 4, "fMask @4");
    assert!(core::mem::offset_of!(MenuItemInfo, f_type) == 8, "fType @8");
    assert!(
        core::mem::offset_of!(MenuItemInfo, f_state) == 12,
        "fState @12"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, w_id) == 16, "wID @16");
    assert!(
        core::mem::offset_of!(MenuItemInfo, submenu_handle) == 24,
        "hSubMenu @24"
    );
    assert!(
        core::mem::offset_of!(MenuItemInfo, type_data_ptr) == 56,
        "dwTypeData @56"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, cch) == 64, "cch @64");
    assert!(
        core::mem::offset_of!(MenuItemInfo, item_bitmap) == 72,
        "hbmpItem @72"
    );
};

/// Win64 `TRACKMOUSEEVENT` (winuser.h): `DWORD cbSize` @0x00, `DWORD dwFlags`
/// @0x04, `HWND hwndTrack` @0x08, `DWORD dwHoverTime` @0x10, [pad] @0x14 —
/// 24 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct TrackMouseEvent {
    pub(crate) cb_size: u32,
    pub(crate) flags: u32,
    pub(crate) track_window_handle: u64,
    pub(crate) hover_time: u32,
    /// Trailing alignment padding (20 payload bytes → 24).
    pub(crate) _pad: [u8; 4],
}

/// Compile-time layout check for [`TrackMouseEvent`].
const _: () = {
    assert!(
        core::mem::size_of::<TrackMouseEvent>() == 24,
        "TRACKMOUSEEVENT must be 24 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, cb_size) == 0,
        "cbSize @0"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, flags) == 4,
        "dwFlags @4"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, track_window_handle) == 8,
        "hwndTrack @8"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, hover_time) == 16,
        "dwHoverTime @16"
    );
};

// ── RawInput family (winuser.h) ───────────────────────────────────────────
//
// WIE *synthesizes* the RAWINPUT payloads (macOS exposes no raw HID stream), but
// the structures themselves are real Win64 layouts: a guest re-walks them with
// `NEXTRAWINPUTBLOCK` (winuser.h:6403) and reads them field-by-field, so every
// offset below is pinned against the mingw headers. See
// `crate::user32::raw_input` for the synthesis caveat and the handler lane.
//
// Authoritative header: mingw-w64 `winuser.h`; every x64 size/offset below was
// extracted from GCC 16.2.0 DWARF for `x86_64-w64-mingw32` and re-proved by a
// `_Static_assert` probe over the same headers.

/// HID generic-desktop usage page (hidusage.h:36, `HID_USAGE_PAGE_GENERIC`).
pub(crate) const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
/// HID generic-desktop mouse usage (hidusage.h:53, `HID_USAGE_GENERIC_MOUSE`).
pub(crate) const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
/// HID generic-desktop keyboard usage (hidusage.h:56,
/// `HID_USAGE_GENERIC_KEYBOARD`).
pub(crate) const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;

/// `GET_RAWINPUT_CODE_WPARAM` / `wParam` code: foreground input (winuser.h:6296).
pub(crate) const RIM_INPUT: u32 = 0;
/// `wParam` code: input sink (background-only) delivery (winuser.h:6297).
pub(crate) const RIM_INPUTSINK: u32 = 1;

/// `RAWINPUTHEADER.dwType` = mouse (winuser.h:6308).
pub(crate) const RIM_TYPE_MOUSE: u32 = 0;
/// `RAWINPUTHEADER.dwType` = keyboard (winuser.h:6309).
pub(crate) const RIM_TYPE_KEYBOARD: u32 = 1;
/// `RAWINPUTHEADER.dwType` = HID (winuser.h:6310).
pub(crate) const RIM_TYPE_HID: u32 = 2;

/// `GetRawInputData` command: the whole record, header included (winuser.h:6405).
pub(crate) const RID_INPUT: u32 = 0x1000_0003;
/// `GetRawInputData` command: the header alone (winuser.h:6406).
pub(crate) const RID_HEADER: u32 = 0x1000_0005;
/// `GetRawInputDeviceInfo` command: preparsed data (winuser.h:6412).
pub(crate) const RIDI_PREPARSEDDATA: u32 = 0x2000_0005;
/// `GetRawInputDeviceInfo` command: the device path string (winuser.h:6413).
pub(crate) const RIDI_DEVICENAME: u32 = 0x2000_0007;
/// `GetRawInputDeviceInfo` command: the `RID_DEVICE_INFO` struct (winuser.h:6414).
pub(crate) const RIDI_DEVICEINFO: u32 = 0x2000_000b;

/// `RAWINPUTDEVICE.dwFlags`: unregister the class (winuser.h:6467).
pub(crate) const RIDEV_REMOVE: u32 = 0x0000_0001;
/// `RAWINPUTDEVICE.dwFlags`: suppress the legacy `WM_MOUSE*`/`WM_KEY*`
/// messages for this class (winuser.h:6468).
pub(crate) const RIDEV_EXCLUDE: u32 = 0x0000_0010;
/// `RAWINPUTDEVICE.dwFlags`: `usUsage` is a page-level mask (winuser.h:6469).
pub(crate) const RIDEV_PAGEONLY: u32 = 0x0000_0020;
/// `RAWINPUTDEVICE.dwFlags`: deliver only to a background window that does not
/// have focus (winuser.h:6471).
pub(crate) const RIDEV_INPUTSINK: u32 = 0x0000_0100;
/// Mask of the mutually exclusive `RIDEV_EX*` bits (winuser.h:6478).
pub(crate) const RIDEV_EXMODEMASK: u32 = 0x0000_00F0;

/// Win64 `sizeof(RAWINPUTHEADER)` — winuser.h:6300-6305.
pub(crate) const RAW_INPUT_HEADER_SIZE: u32 = 24;
/// Win64 `sizeof(RAWMOUSE)` — winuser.h:6314-6327.
pub(crate) const RAW_MOUSE_SIZE: u32 = 24;
/// Win64 `sizeof(RAWKEYBOARD)` — winuser.h:6361-6368.
pub(crate) const RAW_KEYBOARD_SIZE: u32 = 16;
/// Win64 `sizeof(RAWHID)` (the two `DWORD` header fields plus `bRawData[1]`) —
/// winuser.h:6381-6385.
pub(crate) const RAW_HID_SIZE: u32 = 12;
/// Win64 `sizeof(RAWINPUT)` — winuser.h:6387-6394.
pub(crate) const RAW_INPUT_SIZE: u32 = 48;
/// Win64 `sizeof(RAWINPUTDEVICE)` — winuser.h:6457-6462.
pub(crate) const RAW_INPUT_DEVICE_SIZE: u32 = 16;
/// Win64 `sizeof(RAWINPUTDEVICELIST)` — winuser.h:6491-6494.
pub(crate) const RAW_INPUT_DEVICE_LIST_SIZE: u32 = 16;
/// Win64 `sizeof(RID_DEVICE_INFO)` — winuser.h:6441-6449.
pub(crate) const RID_DEVICE_INFO_SIZE: u32 = 32;

/// Win64 `RAWKEYBOARD` scan-code flag: key-down transition (winuser.h:6373).
pub(crate) const RI_KEY_MAKE: u16 = 0;
/// Win64 `RAWKEYBOARD` scan-code flag: key-up transition (winuser.h:6374).
pub(crate) const RI_KEY_BREAK: u16 = 1;
/// Win64 `RAWMOUSE.usFlags`: relative movement (winuser.h:6352).
pub(crate) const MOUSE_MOVE_RELATIVE: u16 = 0;
/// Win64 `RAWMOUSE.usFlags`: absolute movement (winuser.h:6353).
pub(crate) const MOUSE_MOVE_ABSOLUTE: u16 = 1;
/// Win64 `RAWMOUSE` left-button-down (winuser.h:6330).
pub(crate) const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
/// Win64 `RAWMOUSE` left-button-up (winuser.h:6331).
pub(crate) const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
/// Win64 `RAWMOUSE` right-button-down (winuser.h:6332).
pub(crate) const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
/// Win64 `RAWMOUSE` right-button-up (winuser.h:6333).
pub(crate) const RI_MOUSE_RIGHT_BUTTON_UP: u16 = 0x0008;
/// Win64 `RAWMOUSE` middle-button-down (winuser.h:6334).
pub(crate) const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
/// Win64 `RAWMOUSE` middle-button-up (winuser.h:6335).
pub(crate) const RI_MOUSE_MIDDLE_BUTTON_UP: u16 = 0x0020;
/// Win64 `RAWMOUSE` vertical wheel delta (winuser.h:6340).
pub(crate) const RI_MOUSE_WHEEL: u16 = 0x0400;
/// Win64 `RAWMOUSE` horizontal wheel delta (winuser.h:6342).
pub(crate) const RI_MOUSE_HWHEEL: u16 = 0x0800;

/// `sizeof(RAWINPUT)` as a `usize` for slice lengths.
pub(crate) const RAW_INPUT_SIZE_USIZE: usize = 48;

/// Packed `GetRawInputBuffer` record size for one keyboard event:
/// `sizeof(RAWINPUTHEADER) + sizeof(RAWKEYBOARD)`, unaligned (winuser.h:6403
/// applies `RAWINPUT_ALIGN`, a no-op at 40 on x64).
pub(crate) const RAW_INPUT_KEYBOARD_RECORD_SIZE: u32 = RAW_INPUT_HEADER_SIZE + RAW_KEYBOARD_SIZE;
/// Packed `GetRawInputBuffer` record size for one mouse event:
/// `sizeof(RAWINPUTHEADER) + sizeof(RAWMOUSE)`.
pub(crate) const RAW_INPUT_MOUSE_RECORD_SIZE: u32 = RAW_INPUT_HEADER_SIZE + RAW_MOUSE_SIZE;

/// Win64 `RAWINPUTHEADER` (winuser.h:6300-6305): `DWORD dwType` @0x00, `DWORD
/// dwSize` @0x04, `HANDLE hDevice` @0x08, `WPARAM wParam` @0x10 — 24 bytes,
/// align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawInputHeader {
    /// `dwType` — `RIM_TYPE_MOUSE` / `RIM_TYPE_KEYBOARD` / `RIM_TYPE_HID`.
    pub(crate) device_type: u32,
    /// `dwSize` — size of this record, header included. Guest code chains with
    /// `RAWINPUT_ALIGN(base + dwSize)`, so it is the packed size, not
    /// `sizeof(RAWINPUT)`.
    pub(crate) size: u32,
    /// `hDevice` — the synthesizing device handle.
    pub(crate) device: u64,
    /// `wParam` — `RIM_INPUT` or `RIM_INPUTSINK` (only set for the `WM_INPUT`
    /// message, not for the buffer).
    pub(crate) wparam: u64,
}

/// Win64 `RAWMOUSE` (winuser.h:6314-6327): `USHORT usFlags` @0x00, the
/// anonymous `{ULONG ulButtons; {USHORT usButtonFlags; USHORT usButtonData;}}`
/// union @0x04, `ULONG ulRawButtons` @0x08, `LONG lLastX` @0x0C, `LONG lLastY`
/// @0x10, `ULONG ulExtraInformation` @0x14 — 24 bytes, align 4.
///
/// The union is ULONG-aligned, so it starts at +4 (two pad bytes follow
/// `usFlags`), not at +2. `button_flags`/`button_data` together ARE the
/// `ulButtons` alias.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawMouse {
    /// `usFlags` — `MOUSE_MOVE_ABSOLUTE` / `MOUSE_MOVE_RELATIVE` + the
    /// `MOUSE_*` attribute bits.
    pub(crate) mouse_flags: u16,
    /// Win64 alignment padding: the `{ULONG ulButtons; {USHORT usButtonFlags;
    /// USHORT usButtonData;}}` union is ULONG-aligned, so it starts at +0x04.
    pub(crate) _flags_pad: [u8; 2],
    /// `usButtonFlags` — the `RI_MOUSE_*` transition bits.
    pub(crate) button_flags: u16,
    /// `usButtonData` — wheel delta / button-4-5 data.
    pub(crate) button_data: u16,
    /// `ulRawButtons` — button bitmask at the time of the report.
    pub(crate) raw_buttons: u32,
    /// `lLastX` — absolute X, or signed movement when `usFlags` is relative.
    pub(crate) last_x: i32,
    /// `lLastY` — absolute Y, or signed movement when `usFlags` is relative.
    pub(crate) last_y: i32,
    /// `ulExtraInformation` — device-specific extra data.
    pub(crate) extra_information: u32,
}

/// Win64 `RAWKEYBOARD` (winuser.h:6361-6368): `USHORT MakeCode` @0x00, `USHORT
/// Flags` @0x02, `USHORT Reserved` @0x04, `USHORT VKey` @0x06, `UINT Message`
/// @0x08, `ULONG ExtraInformation` @0x0C — 16 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawKeyboard {
    /// `MakeCode` — the keyboard scan code.
    pub(crate) make_code: u16,
    /// `Flags` — the `RI_KEY_*` transition bits.
    pub(crate) flags: u16,
    /// `Reserved` — always 0 in practice.
    pub(crate) reserved: u16,
    /// `VKey` — the translated virtual-key code.
    pub(crate) vkey: u16,
    /// `Message` — the `WM_KEYDOWN`/`WM_KEYUP`/`WM_SYS*` equivalent.
    pub(crate) message: u32,
    /// `ExtraInformation` — device-specific extra data.
    pub(crate) extra_information: u32,
}

/// Win64 `RAWINPUT` (winuser.h:6387-6394): `RAWINPUTHEADER header` @0x00 and the
/// `{RAWMOUSE mouse; RAWKEYBOARD keyboard; RAWHID hid;}` union @0x18 — 48 bytes,
/// align 8.
///
/// Win32 spells that union as a C union; a `#[repr(C)]` byte array of the same
/// size is byte-identical and lets the `KnownLayout` machinery we use for every
/// other guest struct apply. The widest member is `RAWMOUSE` (24 bytes), so
/// 24 + 24 is already 8-aligned and `sizeof(RAWINPUT)` is 48 with no tail
/// padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawInput {
    /// `header` — @0x00.
    pub(crate) header: RawInputHeader,
    /// The payload union — @0x18, 24 bytes.
    pub(crate) data: [u8; 24],
}

/// Win64 `RAWINPUTDEVICE` (winuser.h:6457-6462): `USHORT usUsagePage` @0x00,
/// `USHORT usUsage` @0x02, `DWORD dwFlags` @0x04, `HWND hwndTarget` @0x08 —
/// 16 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawInputDevice {
    /// `usUsagePage` — e.g. `HID_USAGE_PAGE_GENERIC`.
    pub(crate) usage_page: u16,
    /// `usUsage` — the usage, or a page mask under `RIDEV_PAGEONLY`.
    pub(crate) usage: u16,
    /// `dwFlags` — the `RIDEV_*` bits.
    pub(crate) flags: u32,
    /// `hwndTarget` — the window (or `NULL` for input-sink delivery).
    pub(crate) target_window: u64,
}

/// Win64 `RAWINPUTDEVICELIST` (winuser.h:6491-6494): `HANDLE hDevice` @0x00,
/// `DWORD dwType` @0x08 — 16 bytes, align 8 (4 tail pad bytes).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RawInputDeviceList {
    /// `hDevice` — the device handle.
    pub(crate) device: u64,
    /// `dwType` — `RIM_TYPE_MOUSE` / `RIM_TYPE_KEYBOARD` / `RIM_TYPE_HID`.
    pub(crate) device_type: u32,
    /// Win64 tail alignment padding (4 payload bytes → 8).
    pub(crate) _type_pad: [u8; 4],
}

/// Win64 `RID_DEVICE_INFO_MOUSE` (winuser.h:6417-6422) — the `RID_DEVICE_INFO`
/// union member, 16 bytes, every field at its own +0 offset.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RidDeviceInfoMouse {
    /// `dwId` — the mouse id.
    pub(crate) id: u32,
    /// `dwNumberOfButtons` — button count.
    pub(crate) number_of_buttons: u32,
    /// `dwSampleRate` — samples per second (0 = not reported).
    pub(crate) sample_rate: u32,
    /// `fHasHorizontalWheel` — `WINBOOL`, so 4 bytes (not 1) on Win64.
    pub(crate) has_horizontal_wheel: u32,
}

/// Win64 `RID_DEVICE_INFO_KEYBOARD` (winuser.h:6424-6431) — 24 bytes.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RidDeviceInfoKeyboard {
    /// `dwType` — keyboard type.
    pub(crate) keyboard_type: u32,
    /// `dwSubType` — keyboard sub-type.
    pub(crate) keyboard_sub_type: u32,
    /// `dwKeyboardMode` — keyboard mode flags.
    pub(crate) keyboard_mode: u32,
    /// `dwNumberOfFunctionKeys` — F-key count.
    pub(crate) number_of_function_keys: u32,
    /// `dwNumberOfIndicators` — LED/lock-indicator count.
    pub(crate) number_of_indicators: u32,
    /// `dwNumberOfKeysTotal` — total key count.
    pub(crate) number_of_keys_total: u32,
}

/// Win64 `RID_DEVICE_INFO_HID` (winuser.h:6433-6439) — 16 bytes, ending in two
/// `USHORT` usage fields (no tail padding).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RidDeviceInfoHid {
    /// `dwVendorId` — HID vendor id.
    pub(crate) vendor_id: u32,
    /// `dwProductId` — HID product id.
    pub(crate) product_id: u32,
    /// `dwVersionNumber` — HID version.
    pub(crate) version_number: u32,
    /// `usUsagePage` — HID usage page.
    pub(crate) usage_page: u16,
    /// `usUsage` — HID usage.
    pub(crate) usage: u16,
}

/// Win64 `RID_DEVICE_INFO` (winuser.h:6441-6449): `DWORD cbSize` @0x00, `DWORD
/// dwType` @0x04, and the
/// `{RID_DEVICE_INFO_MOUSE mouse; RID_DEVICE_INFO_KEYBOARD keyboard;
/// RID_DEVICE_INFO_HID hid;}` union @0x08 — 32 bytes.
///
/// The union is spelled as a 24-byte payload; the handlers write the concrete
/// member through its own typed view at `self + 8`
/// ([`RidDeviceInfoMouse`], [`RidDeviceInfoKeyboard`], [`RidDeviceInfoHid`]),
/// which is byte-identical to the C union.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct RidDeviceInfo {
    /// `cbSize` — the caller's `sizeof(RID_DEVICE_INFO)`.
    pub(crate) cb_size: u32,
    /// `dwType` — which union member is valid.
    pub(crate) device_type: u32,
    /// The union payload — @0x08, 24 bytes.
    pub(crate) payload: [u8; 24],
}

/// Compile-time layout checks for the whole RawInput family. A drift in any
/// offset here silently corrupts every guest that reads a `RAWINPUT`, so each
/// value is asserted rather than left to a test.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<RawInputHeader>() == 24, "RAWINPUTHEADER = 24");
    assert!(align_of::<RawInputHeader>() == 8, "RAWINPUTHEADER align 8");
    assert!(
        offset_of!(RawInputHeader, device_type) == 0x00,
        "dwType @0x00"
    );
    assert!(offset_of!(RawInputHeader, size) == 0x04, "dwSize @0x04");
    assert!(offset_of!(RawInputHeader, device) == 0x08, "hDevice @0x08");
    assert!(offset_of!(RawInputHeader, wparam) == 0x10, "wParam @0x10");

    assert!(size_of::<RawMouse>() == 24, "RAWMOUSE = 24");
    assert!(align_of::<RawMouse>() == 4, "RAWMOUSE align 4");
    assert!(offset_of!(RawMouse, mouse_flags) == 0x00, "usFlags @0x00");
    assert!(offset_of!(RawMouse, _flags_pad) == 0x02, "pad @0x02");
    assert!(
        offset_of!(RawMouse, button_flags) == 0x04,
        "usButtonFlags @0x04"
    );
    assert!(
        offset_of!(RawMouse, button_data) == 0x06,
        "usButtonData @0x06"
    );
    assert!(
        offset_of!(RawMouse, raw_buttons) == 0x08,
        "ulRawButtons @0x08"
    );
    assert!(offset_of!(RawMouse, last_x) == 0x0C, "lLastX @0x0C");
    assert!(offset_of!(RawMouse, last_y) == 0x10, "lLastY @0x10");
    assert!(
        offset_of!(RawMouse, extra_information) == 0x14,
        "ulExtraInformation @0x14"
    );

    assert!(size_of::<RawKeyboard>() == 16, "RAWKEYBOARD = 16");
    assert!(align_of::<RawKeyboard>() == 4, "RAWKEYBOARD align 4");
    assert!(offset_of!(RawKeyboard, make_code) == 0x00, "MakeCode @0x00");
    assert!(offset_of!(RawKeyboard, flags) == 0x02, "Flags @0x02");
    assert!(offset_of!(RawKeyboard, reserved) == 0x04, "Reserved @0x04");
    assert!(offset_of!(RawKeyboard, vkey) == 0x06, "VKey @0x06");
    assert!(offset_of!(RawKeyboard, message) == 0x08, "Message @0x08");
    assert!(
        offset_of!(RawKeyboard, extra_information) == 0x0C,
        "ExtraInformation @0x0C"
    );

    assert!(size_of::<RawInput>() == 48, "RAWINPUT = 48");
    assert!(align_of::<RawInput>() == 8, "RAWINPUT align 8");
    assert!(offset_of!(RawInput, header) == 0x00, "header @0x00");
    assert!(offset_of!(RawInput, data) == 0x18, "data union @0x18");

    assert!(size_of::<RawInputDevice>() == 16, "RAWINPUTDEVICE = 16");
    assert!(align_of::<RawInputDevice>() == 8, "RAWINPUTDEVICE align 8");
    assert!(
        offset_of!(RawInputDevice, usage_page) == 0x00,
        "usUsagePage @0x00"
    );
    assert!(offset_of!(RawInputDevice, usage) == 0x02, "usUsage @0x02");
    assert!(offset_of!(RawInputDevice, flags) == 0x04, "dwFlags @0x04");
    assert!(
        offset_of!(RawInputDevice, target_window) == 0x08,
        "hwndTarget @0x08"
    );

    assert!(
        size_of::<RawInputDeviceList>() == 16,
        "RAWINPUTDEVICELIST = 16"
    );
    assert!(align_of::<RawInputDeviceList>() == 8, "align 8");
    assert!(
        offset_of!(RawInputDeviceList, device) == 0x00,
        "hDevice @0x00"
    );
    assert!(
        offset_of!(RawInputDeviceList, device_type) == 0x08,
        "dwType @0x08"
    );
    assert!(
        offset_of!(RawInputDeviceList, _type_pad) == 0x0C,
        "dwType tail pad @0x0C"
    );

    assert!(
        size_of::<RidDeviceInfoMouse>() == 16,
        "RID_DEVICE_INFO_MOUSE = 16"
    );
    assert!(
        size_of::<RidDeviceInfoKeyboard>() == 24,
        "RID_DEVICE_INFO_KEYBOARD = 24"
    );
    assert!(
        size_of::<RidDeviceInfoHid>() == 16,
        "RID_DEVICE_INFO_HID = 16"
    );
    assert!(size_of::<RidDeviceInfo>() == 32, "RID_DEVICE_INFO = 32");
    assert!(align_of::<RidDeviceInfo>() == 4, "RID_DEVICE_INFO align 4");
    assert!(offset_of!(RidDeviceInfo, cb_size) == 0x00, "cbSize @0x00");
    assert!(
        offset_of!(RidDeviceInfo, device_type) == 0x04,
        "dwType @0x04"
    );
    assert!(offset_of!(RidDeviceInfo, payload) == 0x08, "union @0x08");

    // Record-format bits a guest reads out of a record and WIE never sets
    // itself; pinned here so the values cannot drift from winuser.h.
    assert!(RI_KEY_MAKE == 0, "RI_KEY_MAKE = 0");
    assert!(RI_KEY_BREAK == 1, "RI_KEY_BREAK = 1");
    assert!(MOUSE_MOVE_RELATIVE == 0, "MOUSE_MOVE_RELATIVE = 0");
    assert!(MOUSE_MOVE_ABSOLUTE == 1, "MOUSE_MOVE_ABSOLUTE = 1");
    assert!(RI_MOUSE_LEFT_BUTTON_DOWN == 0x0001, "left down");
    assert!(RI_MOUSE_LEFT_BUTTON_UP == 0x0002, "left up");
    assert!(RI_MOUSE_RIGHT_BUTTON_DOWN == 0x0004, "right down");
    assert!(RI_MOUSE_RIGHT_BUTTON_UP == 0x0008, "right up");
    assert!(RI_MOUSE_MIDDLE_BUTTON_DOWN == 0x0010, "middle down");
    assert!(RI_MOUSE_MIDDLE_BUTTON_UP == 0x0020, "middle up");
    assert!(RI_MOUSE_WHEEL == 0x0400, "wheel");
    assert!(RI_MOUSE_HWHEEL == 0x0800, "hwheel");
    assert!(HID_USAGE_PAGE_GENERIC == 0x01, "HID_USAGE_PAGE_GENERIC");
    assert!(HID_USAGE_GENERIC_MOUSE == 0x02, "HID_USAGE_GENERIC_MOUSE");
    assert!(
        HID_USAGE_GENERIC_KEYBOARD == 0x06,
        "HID_USAGE_GENERIC_KEYBOARD"
    );
    assert!(RIM_INPUT == 0, "RIM_INPUT");
    assert!(RIM_INPUTSINK == 1, "RIM_INPUTSINK");
    assert!(
        RIM_TYPE_MOUSE == 0 && RIM_TYPE_KEYBOARD == 1 && RIM_TYPE_HID == 2,
        "RIM_TYPE*"
    );
    assert!(RID_INPUT == 0x1000_0003, "RID_INPUT");
    assert!(RID_HEADER == 0x1000_0005, "RID_HEADER");
    assert!(RIDI_PREPARSEDDATA == 0x2000_0005, "RIDI_PREPARSEDDATA");
    assert!(RIDI_DEVICENAME == 0x2000_0007, "RIDI_DEVICENAME");
    assert!(RIDI_DEVICEINFO == 0x2000_000b, "RIDI_DEVICEINFO");
    assert!(RIDEV_REMOVE == 0x0000_0001, "RIDEV_REMOVE");
    assert!(RIDEV_EXCLUDE == 0x0000_0010, "RIDEV_EXCLUDE");
    assert!(RIDEV_PAGEONLY == 0x0000_0020, "RIDEV_PAGEONLY");
    assert!(RIDEV_INPUTSINK == 0x0000_0100, "RIDEV_INPUTSINK");
    assert!(RIDEV_EXMODEMASK == 0x0000_00F0, "RIDEV_EXMODEMASK");
    assert!(RAW_HID_SIZE == 12, "RAWHID = 12");
    assert!(RAW_INPUT_SIZE == 48, "RAWINPUT = 48");
    assert!(RAW_INPUT_SIZE_USIZE == 48, "RAWINPUT = 48 bytes");

    // Packed record arithmetic the GetRawInputBuffer packer relies on.
    assert!(RAW_INPUT_KEYBOARD_RECORD_SIZE == 40, "kbd record = 40");
    assert!(RAW_INPUT_MOUSE_RECORD_SIZE == 48, "mouse record = 48");
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for view round-trips.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    /// Read the raw guest bytes at `va` (bypasses the typed views).
    fn raw_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(va, &mut bytes).expect("read raw bytes");
        bytes
    }
    #[test]
    fn wnd_class_ex_write_places_every_field_at_the_pinned_offsets() {
        let mut engine = test_engine();
        let va = 0x7000_u64;
        with_typed_write::<WndClassEx, _, _>(&mut engine, va, |wc| {
            wc.cb_size = 80;
            wc.style = 0x0000_0002;
            wc.window_proc = 0x1111_2222_3333_4444;
            wc.cb_cls_extra = 3;
            wc.cb_wnd_extra = 4;
            wc.instance_handle = 0x7000_0000_0000_1000;
            wc.icon_handle = 0x6600_0001;
            wc.cursor_handle = 0x6600_0002;
            wc.background_brush = 0x6600_0507;
            wc.menu_name = 0x201;
            wc.class_name_ptr = 0x5000;
            wc.small_icon_handle = 0x6600_0003;
            Ok(())
        })
        .expect("typed WNDCLASSEXW write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes(), "cbSize");
        assert_eq!(&bytes[4..8], &0x0000_0002_u32.to_le_bytes(), "style");
        assert_eq!(&bytes[8..16], &0x1111_2222_3333_4444_u64.to_le_bytes());
        assert_eq!(&bytes[16..20], &3_i32.to_le_bytes(), "cbClsExtra");
        assert_eq!(&bytes[20..24], &4_i32.to_le_bytes(), "cbWndExtra");
        assert_eq!(&bytes[24..32], &0x7000_0000_0000_1000_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x6600_0001_u64.to_le_bytes(), "hIcon");
        assert_eq!(&bytes[40..48], &0x6600_0002_u64.to_le_bytes(), "hCursor");
        assert_eq!(
            &bytes[48..56],
            &0x6600_0507_u64.to_le_bytes(),
            "hbrBackground"
        );
        assert_eq!(
            &bytes[56..64],
            &0x201_u64.to_le_bytes(),
            "lpszMenuName @0x38"
        );
        assert_eq!(&bytes[64..72], &0x5000_u64.to_le_bytes(), "lpszClassName");
        assert_eq!(&bytes[72..80], &0x6600_0003_u64.to_le_bytes(), "hIconSm");
    }

    #[test]
    fn wnd_class_read_preserves_guest_padding_bytes() {
        // WNDCLASS has 4 implicit pad bytes between style and lpfnWndProc.
        // A read view must surface the guest's bytes unchanged.
        let mut engine = test_engine();
        let va = 0x7100_u64;
        let mut bytes = vec![0_u8; 72];
        bytes[0..4].copy_from_slice(&0x0000_0001_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        bytes[8..16].copy_from_slice(&0x0808_0808_0808_0808_u64.to_le_bytes());
        bytes[0x40..0x48].copy_from_slice(&0x5000_u64.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write raw WNDCLASS");
        with_typed_read::<WndClass, _, _>(&mut engine, va, |wc| {
            assert_eq!(wc.style, 1);
            assert_eq!(wc.window_proc, 0x0808_0808_0808_0808);
            assert_eq!(wc.class_name_ptr, 0x5000);
            Ok(())
        })
        .expect("typed WNDCLASS read");
    }

    #[test]
    fn wnd_class_write_zero_fills_padding_between_style_and_wndproc() {
        let mut engine = test_engine();
        let va = 0x7200_u64;
        with_typed_write::<WndClass, _, _>(&mut engine, va, |wc| {
            wc.style = 7;
            wc.window_proc = 0x1234_5678_9ABC_DEF0;
            wc.class_name_ptr = 0x6000;
            Ok(())
        })
        .expect("typed WNDCLASS write");
        let bytes = raw_bytes(&mut engine, va, 72);
        assert_eq!(&bytes[0..4], &7_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "style→lpfnWndProc padding zeroed");
        assert_eq!(&bytes[8..16], &0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0x6000_u64.to_le_bytes());
    }

    #[test]
    fn create_struct_write_matches_hand_written_byte_pattern() {
        let mut engine = test_engine();
        let va = 0x7300_u64;
        with_typed_write::<CreateStruct, _, _>(&mut engine, va, |cs| {
            cs.create_params = 0xAAAA;
            cs.instance_handle = 0xBBBB;
            cs.menu_handle = 0xCCCC;
            cs.parent_handle = 0xDDDD;
            cs.cy = 480;
            cs.cx = 640;
            cs.y = 20;
            cs.x = 10;
            cs.style = 0x10CF0000;
            cs.name_ptr = 0x5000;
            cs.class_ptr = 0x5100;
            cs.extended_style = 0x0000_0100;
            Ok(())
        })
        .expect("typed CREATESTRUCT write");
        let bytes = raw_bytes(&mut engine, va, 0x50);
        let mut expected = vec![0_u8; 0x50];
        expected[0x00..0x08].copy_from_slice(&0xAAAA_u64.to_le_bytes());
        expected[0x08..0x10].copy_from_slice(&0xBBBB_u64.to_le_bytes());
        expected[0x10..0x18].copy_from_slice(&0xCCCC_u64.to_le_bytes());
        expected[0x18..0x20].copy_from_slice(&0xDDDD_u64.to_le_bytes());
        expected[0x20..0x24].copy_from_slice(&480_i32.to_le_bytes());
        expected[0x24..0x28].copy_from_slice(&640_i32.to_le_bytes());
        expected[0x28..0x2C].copy_from_slice(&20_i32.to_le_bytes());
        expected[0x2C..0x30].copy_from_slice(&10_i32.to_le_bytes());
        expected[0x30..0x34].copy_from_slice(&0x10CF0000_u32.to_le_bytes());
        // bytes 0x34..0x38: style→lpszName alignment padding, zero.
        expected[0x38..0x40].copy_from_slice(&0x5000_u64.to_le_bytes());
        expected[0x40..0x48].copy_from_slice(&0x5100_u64.to_le_bytes());
        expected[0x48..0x4C].copy_from_slice(&0x0000_0100_u32.to_le_bytes());
        // bytes 0x4C..0x50: trailing alignment padding, zero.
        assert_eq!(bytes, expected, "CREATESTRUCT byte pattern drift");
    }

    #[test]
    fn rect_and_point_read_write_round_trip_and_stage() {
        let mut engine = test_engine();
        // WinRect write/read round trip at an aligned address.
        with_typed_write::<WinRect, _, _>(&mut engine, 0x7400, |rect| {
            rect.left = -5;
            rect.top = 10;
            rect.right = 320;
            rect.bottom = 200;
            Ok(())
        })
        .expect("typed RECT write");
        with_typed_read::<WinRect, _, _>(&mut engine, 0x7400, |rect| {
            assert_eq!(
                (rect.left, rect.top, rect.right, rect.bottom),
                (-5, 10, 320, 200)
            );
            Ok(())
        })
        .expect("typed RECT read");

        // WinPoint write at an odd (misaligned) VA: the staging fallback must
        // produce byte-identical output.
        with_typed_write::<WinPoint, _, _>(&mut engine, 0x7501, |point| {
            point.x = 42;
            point.y = -7;
            Ok(())
        })
        .expect("staged typed POINT write");
        let bytes = raw_bytes(&mut engine, 0x7501, 8);
        assert_eq!(&bytes[0..4], &42_i32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-7_i32).to_le_bytes());
    }

    #[test]
    fn menu_item_info_write_preserves_pads_and_all_fields() {
        let mut engine = test_engine();
        let va = 0x7600_u64;
        with_typed_write::<MenuItemInfo, _, _>(&mut engine, va, |info| {
            info.cb_size = 80;
            info.f_mask = 0x20;
            info.f_type = 0;
            info.f_state = 0;
            info.w_id = 0x101;
            info.type_data_ptr = 0x6000;
            info.cch = 12;
            info.item_bitmap = 0x6600_0001;
            Ok(())
        })
        .expect("typed MENUITEMINFO write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes(), "cbSize");
        assert_eq!(&bytes[4..8], &0x20_u32.to_le_bytes(), "fMask");
        assert_eq!(&bytes[16..20], &0x101_u32.to_le_bytes(), "wID");
        assert_eq!(&bytes[20..24], &[0; 4], "wID→hSubMenu padding");
        assert_eq!(&bytes[24..32], &0_u64.to_le_bytes(), "hSubMenu");
        assert_eq!(&bytes[56..64], &0x6000_u64.to_le_bytes(), "dwTypeData");
        assert_eq!(&bytes[64..68], &12_u32.to_le_bytes(), "cch");
        assert_eq!(&bytes[68..72], &[0; 4], "cch→hbmpItem padding");
        assert_eq!(&bytes[72..80], &0x6600_0001_u64.to_le_bytes(), "hbmpItem");
    }

    #[test]
    fn track_mouse_event_read_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7700_u64;
        let mut bytes = vec![0_u8; 24];
        bytes[0..4].copy_from_slice(&24_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&2_u32.to_le_bytes()); // TME_LEAVE
        bytes[8..16].copy_from_slice(&0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        bytes[16..20].copy_from_slice(&500_u32.to_le_bytes());
        engine
            .mem_write(va, &bytes)
            .expect("write raw TRACKMOUSEEVENT");
        with_typed_read::<TrackMouseEvent, _, _>(&mut engine, va, |tme| {
            assert_eq!(tme.cb_size, 24);
            assert_eq!(tme.flags, 2);
            assert_eq!(tme.track_window_handle, 0x1234_5678_9ABC_DEF0);
            assert_eq!(tme.hover_time, 500);
            Ok(())
        })
        .expect("typed TRACKMOUSEEVENT read");
    }

    /// The RAWINPUT record WIE writes into guest memory must be byte-identical
    /// to what a real Win64 guest reads, because the guest re-walks it with
    /// `NEXTRAWINPUTBLOCK` (winuser.h:6403) and `GetRawInputData`. A wrong
    /// offset in this family already cost 7-Zip an infinite recursion once
    /// (see CLAUDE.md), so every offset is asserted against the raw bytes.
    #[test]
    fn raw_input_keyboard_record_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7400_u64;
        // sizeof(RAWINPUTHEADER)=24 + sizeof(RAWKEYBOARD)=16 = 40 packed bytes.
        with_typed_write::<RawInputHeader, _, _>(&mut engine, va, |header| {
            header.device_type = RIM_TYPE_KEYBOARD;
            header.size = RAW_INPUT_KEYBOARD_RECORD_SIZE;
            header.device = 0x6600_0501;
            header.wparam = 0;
            Ok(())
        })
        .expect("typed RAWINPUTHEADER write");
        with_typed_write::<RawKeyboard, _, _>(&mut engine, va + 24, |kbd| {
            kbd.make_code = 0x1E;
            kbd.flags = RI_KEY_BREAK;
            kbd.reserved = 0;
            kbd.vkey = 0x41;
            kbd.message = 0x0101; // WM_KEYUP
            kbd.extra_information = 0;
            Ok(())
        })
        .expect("typed RAWKEYBOARD write");

        let bytes = raw_bytes(&mut engine, va, 40);
        assert_eq!(&bytes[0..4], &1_u32.to_le_bytes(), "header.dwType");
        assert_eq!(&bytes[4..8], &40_u32.to_le_bytes(), "header.dwSize");
        assert_eq!(&bytes[8..16], &0x6600_0501_u64.to_le_bytes(), "hDevice");
        assert_eq!(&bytes[16..24], &0_u64.to_le_bytes(), "wParam");
        assert_eq!(&bytes[24..26], &0x1E_u16.to_le_bytes(), "MakeCode");
        assert_eq!(&bytes[26..28], &1_u16.to_le_bytes(), "Flags");
        assert_eq!(&bytes[28..30], &0_u16.to_le_bytes(), "Reserved");
        assert_eq!(&bytes[30..32], &0x41_u16.to_le_bytes(), "VKey");
        assert_eq!(&bytes[32..36], &0x0101_u32.to_le_bytes(), "Message");
        assert_eq!(&bytes[36..40], &0_u32.to_le_bytes(), "ExtraInformation");
    }

    /// The mouse payload is the widest member of the `RAWINPUT` union
    /// (`RAWMOUSE` = 24 bytes, not 20): the anonymous
    /// `{ULONG ulButtons; {USHORT usButtonFlags; USHORT usButtonData;}}` union
    /// is ULONG-aligned, so it starts at +4 and not at +2.
    #[test]
    fn raw_input_mouse_record_places_the_button_union_at_offset_four() {
        let mut engine = test_engine();
        let va = 0x7500_u64;
        with_typed_write::<RawInputHeader, _, _>(&mut engine, va, |header| {
            header.device_type = RIM_TYPE_MOUSE;
            header.size = RAW_INPUT_MOUSE_RECORD_SIZE;
            header.device = 0x6600_0502;
            header.wparam = 0;
            Ok(())
        })
        .expect("typed RAWINPUTHEADER write");
        with_typed_write::<RawMouse, _, _>(&mut engine, va + 24, |mouse| {
            mouse.mouse_flags = MOUSE_MOVE_ABSOLUTE;
            mouse.button_flags = RI_MOUSE_LEFT_BUTTON_DOWN;
            mouse.button_data = 0;
            mouse.raw_buttons = 0;
            mouse.last_x = -3;
            mouse.last_y = 7;
            mouse.extra_information = 0;
            Ok(())
        })
        .expect("typed RAWMOUSE write");

        let bytes = raw_bytes(&mut engine, va, 48);
        assert_eq!(&bytes[0..4], &0_u32.to_le_bytes(), "header.dwType");
        assert_eq!(&bytes[4..8], &48_u32.to_le_bytes(), "header.dwSize");
        // usFlags @+24, 2 pad bytes, then the ULONG-aligned union @+28.
        assert_eq!(&bytes[24..26], &1_u16.to_le_bytes(), "RAWMOUSE.usFlags");
        assert_eq!(&bytes[26..28], &[0, 0], "usFlags→union padding");
        assert_eq!(&bytes[28..30], &1_u16.to_le_bytes(), "usButtonFlags");
        assert_eq!(&bytes[30..32], &0_u16.to_le_bytes(), "usButtonData");
        assert_eq!(&bytes[32..36], &0_u32.to_le_bytes(), "ulRawButtons");
        assert_eq!(&bytes[36..40], &(-3_i32).to_le_bytes(), "lLastX");
        assert_eq!(&bytes[40..44], &7_i32.to_le_bytes(), "lLastY");
        assert_eq!(&bytes[44..48], &0_u32.to_le_bytes(), "ulExtraInformation");
    }

    /// `RAWINPUTDEVICE` / `RAWINPUTDEVICELIST` / `RID_DEVICE_INFO` are the
    /// structures `RegisterRawInputDevices` reads and
    /// `GetRawInputDeviceList`/`GetRawInputDeviceInfo` write.
    #[test]
    fn raw_input_device_structs_match_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7600_u64;
        with_typed_write::<RawInputDevice, _, _>(&mut engine, va, |device| {
            device.usage_page = HID_USAGE_PAGE_GENERIC;
            device.usage = HID_USAGE_GENERIC_KEYBOARD;
            device.flags = RIDEV_INPUTSINK;
            device.target_window = 0x1234;
            Ok(())
        })
        .expect("typed RAWINPUTDEVICE write");
        let bytes = raw_bytes(&mut engine, va, 16);
        assert_eq!(&bytes[0..2], &0x01_u16.to_le_bytes(), "usUsagePage");
        assert_eq!(&bytes[2..4], &0x06_u16.to_le_bytes(), "usUsage");
        assert_eq!(&bytes[4..8], &0x100_u32.to_le_bytes(), "dwFlags");
        assert_eq!(&bytes[8..16], &0x1234_u64.to_le_bytes(), "hwndTarget");

        with_typed_write::<RawInputDeviceList, _, _>(&mut engine, va, |entry| {
            entry.device = 0x6600_0501;
            entry.device_type = RIM_TYPE_KEYBOARD;
            Ok(())
        })
        .expect("typed RAWINPUTDEVICELIST write");
        let bytes = raw_bytes(&mut engine, va, 16);
        assert_eq!(&bytes[0..8], &0x6600_0501_u64.to_le_bytes(), "hDevice");
        assert_eq!(&bytes[8..12], &1_u32.to_le_bytes(), "dwType");
        assert_eq!(&bytes[12..16], &[0; 4], "dwType tail padding");

        with_typed_write::<RidDeviceInfo, _, _>(&mut engine, va, |info| {
            info.cb_size = RID_DEVICE_INFO_SIZE;
            info.device_type = RIM_TYPE_KEYBOARD;
            info.payload = [0; 24];
            Ok(())
        })
        .expect("typed RID_DEVICE_INFO write");
        with_typed_write::<RidDeviceInfoKeyboard, _, _>(&mut engine, va + 8, |kbd| {
            kbd.keyboard_type = 1;
            kbd.keyboard_sub_type = 4;
            kbd.keyboard_mode = 0;
            kbd.number_of_function_keys = 12;
            kbd.number_of_indicators = 3;
            kbd.number_of_keys_total = 104;
            Ok(())
        })
        .expect("typed RID_DEVICE_INFO_KEYBOARD write");
        let bytes = raw_bytes(&mut engine, va, 32);
        assert_eq!(&bytes[0..4], &32_u32.to_le_bytes(), "cbSize");
        assert_eq!(&bytes[4..8], &1_u32.to_le_bytes(), "dwType");
        assert_eq!(&bytes[8..12], &1_u32.to_le_bytes(), "keyboard.dwType");
        assert_eq!(&bytes[12..16], &4_u32.to_le_bytes(), "dwSubType");
        assert_eq!(&bytes[20..24], &12_u32.to_le_bytes(), "nFunctionKeys");
        assert_eq!(&bytes[24..28], &3_u32.to_le_bytes(), "nIndicators");
        assert_eq!(&bytes[28..32], &104_u32.to_le_bytes(), "nKeysTotal");
    }

    /// The fixed 48-byte `RAWINPUT` a guest allocates must expose the payload
    /// union at +24 — a guest that indexes `ri.data.keyboard` at +24 finds the
    /// keyboard it wrote through `GetRawInputData`.
    #[test]
    fn raw_input_fixed_blob_places_the_payload_union_at_offset_24() {
        let mut engine = test_engine();
        let va = 0x7700_u64;
        with_typed_write::<RawInput, _, _>(&mut engine, va, |input| {
            input.header.device_type = RIM_TYPE_HID;
            input.header.size = RAW_INPUT_SIZE;
            input.header.device = 0x6600_0503;
            input.header.wparam = 0;
            input.data = [0xAB; 24];
            Ok(())
        })
        .expect("typed RAWINPUT write");
        let bytes = raw_bytes(&mut engine, va, RAW_INPUT_SIZE_USIZE);
        assert_eq!(&bytes[0..4], &2_u32.to_le_bytes(), "header.dwType");
        assert_eq!(&bytes[4..8], &48_u32.to_le_bytes(), "header.dwSize");
        assert_eq!(&bytes[8..16], &0x6600_0503_u64.to_le_bytes(), "hDevice");
        assert_eq!(&bytes[16..24], &0_u64.to_le_bytes(), "wParam");
        assert_eq!(&bytes[24..48], &[0xAB; 24], "union @+24");
    }
}
