//! user32 lane: `WndClassEx`, `WndClass`, `CreateStruct`, `WinRect`,
//! `WinPoint`, `MenuItemInfo`, `TrackMouseEvent`.

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
}
