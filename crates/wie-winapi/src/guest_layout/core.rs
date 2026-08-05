//! Z0 core: `Msg` and `WindowPlacement` — the first zerocopy layouts.
//! The const-assert tables pin the Win64 offsets at compile time; the
//! `#[cfg(test)]` module keeps the round-trip tests adjacent to their
//! structs.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Win64 `MSG` (winuser.h): `HWND hwnd` @0, `UINT message` @8, [pad @12],
/// `WPARAM wParam` @16, `LPARAM lParam` @24, `DWORD time` @32, `POINT pt` @36
/// (`x` @36, `y` @40), `DWORD lPrivate` @44 — 48 bytes, align 8.
///
/// The `_pad` field is explicit because zerocopy 0.8's `IntoBytes` derive
/// rejects implicit padding; the zero-fill write path keeps it zeroed, exactly
/// as the old per-field handler cleared it.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct Msg {
    pub(crate) hwnd: u64,
    pub(crate) message: u32,
    /// Win64 alignment padding between `message` and `wParam`.
    pub(crate) _pad: [u8; 4],
    pub(crate) wparam: u64,
    pub(crate) lparam: u64,
    pub(crate) time: u32,
    pub(crate) pt_x: i32,
    pub(crate) pt_y: i32,
    /// Private field (winuser.h `lPrivate`); zero-filled like the padding.
    pub(crate) l_private: u32,
}

/// Compile-time layout check for [`Msg`]: any field reorder or wrong width
/// breaks the build instead of corrupting guest memory at runtime.
const _: () = {
    assert!(
        core::mem::size_of::<Msg>() == 48,
        "MSG must be 48 bytes on Win64"
    );
    assert!(core::mem::offset_of!(Msg, hwnd) == 0, "MSG.hwnd @0");
    assert!(core::mem::offset_of!(Msg, message) == 8, "MSG.message @8");
    assert!(core::mem::offset_of!(Msg, _pad) == 12, "MSG padding @12");
    assert!(core::mem::offset_of!(Msg, wparam) == 16, "MSG.wParam @16");
    assert!(core::mem::offset_of!(Msg, lparam) == 24, "MSG.lParam @24");
    assert!(core::mem::offset_of!(Msg, time) == 32, "MSG.time @32");
    assert!(core::mem::offset_of!(Msg, pt_x) == 36, "MSG.pt.x @36");
    assert!(core::mem::offset_of!(Msg, pt_y) == 40, "MSG.pt.y @40");
    assert!(
        core::mem::offset_of!(Msg, l_private) == 44,
        "MSG.lPrivate @44"
    );
};

/// Win64 `WINDOWPLACEMENT` (winuser.h, Vista+ — `rcDevice` was removed):
/// `UINT length` @0, `UINT flags` @4, `UINT showCmd` @8, `POINT ptMinPosition`
/// @12 (`x` @12, `y` @16), `POINT ptMaxPosition` @20 (`x` @20, `y` @24), `RECT
/// rcNormalPosition` @28 (`left` @28, `top` @32, `right` @36, `bottom` @40) —
/// 44 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WindowPlacement {
    pub(crate) length: u32,
    pub(crate) flags: u32,
    pub(crate) show_cmd: u32,
    pub(crate) pt_min_x: i32,
    pub(crate) pt_min_y: i32,
    pub(crate) pt_max_x: i32,
    pub(crate) pt_max_y: i32,
    pub(crate) rc_left: i32,
    pub(crate) rc_top: i32,
    pub(crate) rc_right: i32,
    pub(crate) rc_bottom: i32,
}

/// Compile-time layout check for [`WindowPlacement`]. The 44-byte size matches
/// `user32::window::geom::WINDOWPLACEMENT_LENGTH`, which both placement
/// handlers agree on.
const _: () = {
    assert!(
        core::mem::size_of::<WindowPlacement>() == 44,
        "WINDOWPLACEMENT must be 44 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, length) == 0,
        "length @0"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, flags) == 4,
        "flags @4"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, show_cmd) == 8,
        "showCmd @8"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_min_x) == 12,
        "ptMinPosition.x @12"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_min_y) == 16,
        "ptMinPosition.y @16"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_max_x) == 20,
        "ptMaxPosition.x @20"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_max_y) == 24,
        "ptMaxPosition.y @24"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_left) == 28,
        "rcNormalPosition.left @28"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_top) == 32,
        "rcNormalPosition.top @32"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_right) == 36,
        "rcNormalPosition.right @36"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_bottom) == 40,
        "rcNormalPosition.bottom @40"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const MSG_VA: u64 = 0x4000;

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
    fn msg_write_view_zero_fills_padding_and_unset_fields() {
        let mut engine = test_engine();
        with_typed_write::<Msg, _, _>(&mut engine, MSG_VA, |msg| {
            // Deliberately leave wparam/lparam/time/pt/l_private unset: the
            // view starts zeroed (GetStartupInfo semantics).
            msg.hwnd = 0x1122_3344_5566_7788;
            msg.message = 0x0100;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, MSG_VA, 48);
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788_u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0100_u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0], "Win64 MSG padding");
        assert_eq!(&bytes[16..48], &[0; 32], "unset fields + lPrivate");
    }

    #[test]
    fn msg_write_view_matches_hand_written_byte_pattern() {
        let mut engine = test_engine();
        with_typed_write::<Msg, _, _>(&mut engine, MSG_VA, |msg| {
            msg.hwnd = 0x1111_2222_3333_4444;
            msg.message = 0x0F;
            msg.wparam = 0xAAAA_BBBB_CCCC_DDDD;
            msg.lparam = 0xDEAD_BEEF_CAFE_F00D;
            msg.time = 0x1234_5678;
            msg.pt_x = -7;
            msg.pt_y = 99;
            msg.l_private = 0;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, MSG_VA, 48);
        let mut expected = vec![0_u8; 48];
        expected[0..8].copy_from_slice(&0x1111_2222_3333_4444_u64.to_le_bytes());
        expected[8..12].copy_from_slice(&0x0F_u32.to_le_bytes());
        // bytes 12..16: alignment padding, zero.
        expected[16..24].copy_from_slice(&0xAAAA_BBBB_CCCC_DDDD_u64.to_le_bytes());
        expected[24..32].copy_from_slice(&0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes());
        expected[32..36].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        expected[36..40].copy_from_slice(&(-7_i32).to_le_bytes());
        expected[40..44].copy_from_slice(&99_i32.to_le_bytes());
        // bytes 44..48: lPrivate, zero.
        assert_eq!(bytes, expected, "MSG byte pattern drift");
    }

    #[test]
    fn msg_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        // Write a raw MSG with nonzero padding: the read view must preserve
        // guest bytes exactly (reads do not zero-fill).
        let mut bytes = vec![0_u8; 48];
        bytes[0..8].copy_from_slice(&7_u64.to_le_bytes());
        bytes[8..12].copy_from_slice(&0xABCD_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        bytes[16..24].copy_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&0xF0E0_D0C0_B0A0_9080_u64.to_le_bytes());
        bytes[32..36].copy_from_slice(&0x0101_0101_u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&(-1_i32).to_le_bytes());
        bytes[40..44].copy_from_slice(&(-2_i32).to_le_bytes());
        bytes[44..48].copy_from_slice(&0x77_u32.to_le_bytes());
        engine
            .mem_write(MSG_VA, &bytes)
            .expect("write raw MSG bytes");

        with_typed_read::<Msg, _, _>(&mut engine, MSG_VA, |view| {
            assert_eq!(view.hwnd, 7);
            assert_eq!(view.message, 0xABCD);
            assert_eq!(view.wparam, 0x0102_0304_0506_0708);
            assert_eq!(view.lparam, 0xF0E0_D0C0_B0A0_9080);
            assert_eq!(view.time, 0x0101_0101);
            assert_eq!(view.pt_x, -1);
            assert_eq!(view.pt_y, -2);
            assert_eq!(view.l_private, 0x77);
            Ok(())
        })
        .expect("typed read");
    }

    #[test]
    fn msg_misaligned_guest_va_stages_instead_of_erroring() {
        // An odd address cannot be borrowed in place (align 8): the helper
        // must stage into an aligned host buffer and produce identical bytes.
        let mut engine = test_engine();
        let odd_va = MSG_VA + 1;
        with_typed_write::<Msg, _, _>(&mut engine, odd_va, |msg| {
            msg.hwnd = 0x1234_5678_9ABC_DEF0;
            msg.message = 0x111;
            msg.wparam = 5;
            msg.lparam = 6;
            msg.time = 7;
            msg.pt_x = 8;
            msg.pt_y = 9;
            Ok(())
        })
        .expect("staged typed write");
        let bytes = raw_bytes(&mut engine, odd_va, 48);
        assert_eq!(&bytes[0..8], &0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x111_u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &5_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &6_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &7_u32.to_le_bytes());
        assert_eq!(&bytes[36..40], &8_i32.to_le_bytes());
        assert_eq!(&bytes[40..44], &9_i32.to_le_bytes());
        assert_eq!(&bytes[12..16], &[0; 4], "padding stays zero when staged");
        assert_eq!(&bytes[44..48], &[0; 4], "lPrivate stays zero when staged");
    }

    #[test]
    fn window_placement_write_zero_fills_points_and_flags() {
        let mut engine = test_engine();
        let va = 0x5000_u64;
        with_typed_write::<WindowPlacement, _, _>(&mut engine, va, |placement| {
            placement.length = 44;
            placement.show_cmd = 1;
            placement.rc_left = 10;
            placement.rc_top = 20;
            placement.rc_right = 210;
            placement.rc_bottom = 120;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, va, 44);
        assert_eq!(&bytes[0..4], &44_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "flags must be zero");
        assert_eq!(&bytes[8..12], &1_u32.to_le_bytes());
        assert_eq!(&bytes[12..28], &[0; 16], "min/max positions zero");
        assert_eq!(&bytes[28..32], &10_i32.to_le_bytes());
        assert_eq!(&bytes[32..36], &20_i32.to_le_bytes());
        assert_eq!(&bytes[36..40], &210_i32.to_le_bytes());
        assert_eq!(&bytes[40..44], &120_i32.to_le_bytes());
    }

    #[test]
    fn window_placement_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x5000_u64;
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&44_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&3_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1_i32.to_le_bytes());
        bytes[16..20].copy_from_slice(&2_i32.to_le_bytes());
        bytes[20..24].copy_from_slice(&3_i32.to_le_bytes());
        bytes[24..28].copy_from_slice(&4_i32.to_le_bytes());
        bytes[28..32].copy_from_slice(&100_i32.to_le_bytes());
        bytes[32..36].copy_from_slice(&200_i32.to_le_bytes());
        bytes[36..40].copy_from_slice(&300_i32.to_le_bytes());
        bytes[40..44].copy_from_slice(&400_i32.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write raw placement");

        with_typed_read::<WindowPlacement, _, _>(&mut engine, va, |view| {
            assert_eq!(view.length, 44);
            assert_eq!(view.flags, 0);
            assert_eq!(view.show_cmd, 3);
            assert_eq!(view.pt_min_x, 1);
            assert_eq!(view.pt_min_y, 2);
            assert_eq!(view.pt_max_x, 3);
            assert_eq!(view.pt_max_y, 4);
            assert_eq!(view.rc_left, 100);
            assert_eq!(view.rc_top, 200);
            assert_eq!(view.rc_right, 300);
            assert_eq!(view.rc_bottom, 400);
            Ok(())
        })
        .expect("typed read");
    }

    #[test]
    fn window_placement_misaligned_stage_read_preserves_bytes() {
        let mut engine = test_engine();
        let odd_va = 0x5000_u64 + 1;
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&44_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&(-5_i32).to_le_bytes());
        engine
            .mem_write(odd_va, &bytes)
            .expect("write raw placement at odd address");
        with_typed_read::<WindowPlacement, _, _>(&mut engine, odd_va, |view| {
            assert_eq!(view.length, 44);
            assert_eq!(view.show_cmd, 1);
            assert_eq!(view.rc_left, -5);
            assert_eq!(view.rc_bottom, 0);
            Ok(())
        })
        .expect("staged typed read");
    }
}
