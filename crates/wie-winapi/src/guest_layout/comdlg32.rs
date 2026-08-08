//! comdlg32 lane: `OpenFileName` and `FindReplace` — the common-dialog
//! structs. The Z1 print-lane dialog structs live in `print_lane`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// --- comdlg32 lane: OPENFILENAME / FINDREPLACE (the dialog structs) --------

/// Win64 `OPENFILENAMEW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HINSTANCE hInstance` @16, `LPCWSTR lpstrFilter` @24, `LPWSTR
/// lpstrCustomFilter` @32, `DWORD nMaxCustFilter` @40, `nFilterIndex` @44,
/// `LPWSTR lpstrFile` @48, `DWORD nMaxFile` @56, [pad @60], `LPWSTR
/// lpstrFileTitle` @64, `DWORD nMaxFileTitle` @72, [pad @76], `LPCWSTR
/// lpstrInitialDir` @80, `lpstrTitle` @88, `DWORD Flags` @96, `WORD
/// nFileOffset` @100, `nFileExtension` @102, `LPCWSTR lpstrDefExt` @104,
/// `LPARAM lCustData` @112, `LPOFNHOOKPROC lpfnHook` @120, `LPCWSTR
/// lpTemplateName` @128, `void* pvReserved` @136, `DWORD dwReserved` @144,
/// `FlagsEx` @148 — 152 bytes, align 8.
///
/// Sizes and offsets verified against mingw-w64 14.0.0 `commdlg.h`, whose
/// `OPENFILENAMEW` carries the Vista+ reserved tail (`pvReserved` /
/// `dwReserved` / `FlagsEx`). The A-variant (`OPENFILENAMEA`) has the
/// identical layout — the strings are ANSI but every offset matches — so the
/// host reads both through this one type. The `_pad` fields follow the
/// `IntoBytes` explicit-padding rule; the read-modify-write path restores the
/// guest's original pad bytes.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct OpenFileName {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before `hwndOwner`.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_instance: u64,
    pub(crate) lpstr_filter: u64,
    pub(crate) lpstr_custom_filter: u64,
    pub(crate) n_max_cust_filter: u32,
    pub(crate) n_filter_index: u32,
    pub(crate) lpstr_file: u64,
    pub(crate) n_max_file: u32,
    /// Win64 alignment padding before `lpstrFileTitle`.
    pub(crate) _pad2: [u8; 4],
    pub(crate) lpstr_file_title: u64,
    pub(crate) n_max_file_title: u32,
    /// Win64 alignment padding before `lpstrInitialDir`.
    pub(crate) _pad3: [u8; 4],
    pub(crate) lpstr_initial_dir: u64,
    pub(crate) lpstr_title: u64,
    pub(crate) flags: u32,
    pub(crate) n_file_offset: u16,
    pub(crate) n_file_extension: u16,
    pub(crate) lpstr_def_ext: u64,
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_hook: u64,
    pub(crate) lp_template_name: u64,
    /// Vista+ reserved tail (mingw-w64 commdlg.h) — read/written whole so the
    /// guest's `pvReserved`/`dwReserved`/`FlagsEx` survive the write-back.
    pub(crate) pv_reserved: u64,
    pub(crate) dw_reserved: u32,
    pub(crate) flags_ex: u32,
}

/// Compile-time layout check for [`OpenFileName`].
const _: () = {
    assert!(
        core::mem::size_of::<OpenFileName>() == 152,
        "OPENFILENAMEW must be 152 bytes on Win64 (mingw-w64 14.0.0)"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad) == 4,
        "OPENFILENAME pad @4"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, h_instance) == 16,
        "hInstance @16"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_filter) == 24,
        "lpstrFilter @24"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_custom_filter) == 32,
        "lpstrCustomFilter @32"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_cust_filter) == 40,
        "nMaxCustFilter @40"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_filter_index) == 44,
        "nFilterIndex @44"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_file) == 48,
        "lpstrFile @48"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_file) == 56,
        "nMaxFile @56"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad2) == 60,
        "OPENFILENAME pad @60"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_file_title) == 64,
        "lpstrFileTitle @64"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_file_title) == 72,
        "nMaxFileTitle @72"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad3) == 76,
        "OPENFILENAME pad @76"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_initial_dir) == 80,
        "lpstrInitialDir @80"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_title) == 88,
        "lpstrTitle @88"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, flags) == 96,
        "Flags @96"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_file_offset) == 100,
        "nFileOffset @100"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_file_extension) == 102,
        "nFileExtension @102"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_def_ext) == 104,
        "lpstrDefExt @104"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, l_cust_data) == 112,
        "lCustData @112"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpfn_hook) == 120,
        "lpfnHook @120"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lp_template_name) == 128,
        "lpTemplateName @128"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, pv_reserved) == 136,
        "pvReserved @136"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, dw_reserved) == 144,
        "dwReserved @144"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, flags_ex) == 148,
        "FlagsEx @148"
    );
};

/// Win64 `FINDREPLACEW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HINSTANCE hInstance` @16, `DWORD Flags` @24, [pad @28],
/// `LPWSTR lpstrFindWhat` @32, `lpstrReplaceWith` @40, `WORD wFindWhatLen`
/// @48, `wReplaceWithLen` @50, [pad @52], `LPARAM lCustData` @56,
/// `LPFRHOOKPROC lpfnHook` @64, `LPCWSTR lpTemplateName` @72 — 80 bytes,
/// align 8 (mingw-w64 14.0.0 commdlg.h; the structure is UNICODE regardless
/// of the A/W suffix of the creating API, so FindTextA/ReplaceTextA still use
/// this type).
///
/// The `_pad` fields follow the `IntoBytes` explicit-padding rule; the
/// read-modify-write paths (the `Flags` write-backs on submit/close) restore
/// the guest's original pad bytes.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FindReplace {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before `hwndOwner`.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_instance: u64,
    pub(crate) flags: u32,
    /// Win64 alignment padding before the first string pointer.
    pub(crate) _pad2: [u8; 4],
    pub(crate) lpstr_find_what: u64,
    pub(crate) lpstr_replace_with: u64,
    pub(crate) w_find_what_len: u16,
    pub(crate) w_replace_with_len: u16,
    /// Win64 alignment padding before `lCustData`.
    pub(crate) _pad3: [u8; 4],
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_hook: u64,
    pub(crate) lp_template_name: u64,
}

/// Compile-time layout check for [`FindReplace`].
const _: () = {
    assert!(
        core::mem::size_of::<FindReplace>() == 80,
        "FINDREPLACEW must be 80 bytes on Win64 (mingw-w64 14.0.0)"
    );
    assert!(
        core::mem::offset_of!(FindReplace, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(FindReplace, _pad) == 4,
        "FINDREPLACE pad @4"
    );
    assert!(
        core::mem::offset_of!(FindReplace, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(FindReplace, h_instance) == 16,
        "hInstance @16"
    );
    assert!(core::mem::offset_of!(FindReplace, flags) == 24, "Flags @24");
    assert!(
        core::mem::offset_of!(FindReplace, _pad2) == 28,
        "FINDREPLACE pad @28"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpstr_find_what) == 32,
        "lpstrFindWhat @32"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpstr_replace_with) == 40,
        "lpstrReplaceWith @40"
    );
    assert!(
        core::mem::offset_of!(FindReplace, w_find_what_len) == 48,
        "wFindWhatLen @48"
    );
    assert!(
        core::mem::offset_of!(FindReplace, w_replace_with_len) == 50,
        "wReplaceWithLen @50"
    );
    assert!(
        core::mem::offset_of!(FindReplace, _pad3) == 52,
        "FINDREPLACE pad @52"
    );
    assert!(
        core::mem::offset_of!(FindReplace, l_cust_data) == 56,
        "lCustData @56"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpfn_hook) == 64,
        "lpfnHook @64"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lp_template_name) == 72,
        "lpTemplateName @72"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod comdlg32_lane_tests {
    use super::*;
    use crate::guest_memory::{
        read_typed_copy, with_typed_read, with_typed_write, write_typed_copy,
    };
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for view round-trips. Each
    /// test builds a fresh engine, so VAs may repeat across tests.
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
    fn open_file_name_round_trip_pins_all_offsets_and_pads() {
        let mut engine = test_engine();
        let va = 0xA100_u64;
        with_typed_write::<OpenFileName, _, _>(&mut engine, va, |ofn| {
            ofn.l_struct_size = 152;
            ofn.hwnd_owner = 0x0102_0304_0506_0708;
            ofn.h_instance = 0x1112_1314_1516_1718;
            ofn.lpstr_filter = 0x2122_2324_2526_2728;
            ofn.lpstr_custom_filter = 0x3132_3334_3536_3738;
            ofn.n_max_cust_filter = 0x1111_2222;
            ofn.n_filter_index = 1;
            ofn.lpstr_file = 0x4142_4344_4546_4748;
            ofn.n_max_file = 260;
            ofn.lpstr_file_title = 0x5152_5354_5556_5758;
            ofn.n_max_file_title = 64;
            ofn.lpstr_initial_dir = 0x6162_6364_6566_6768;
            ofn.lpstr_title = 0x7172_7374_7576_7778;
            ofn.flags = 0x0000_0008;
            ofn.n_file_offset = 9;
            ofn.n_file_extension = 15;
            ofn.lpstr_def_ext = 0x8182_8384_8586_8788;
            ofn.l_cust_data = 0x9192_9394_9596_9798;
            ofn.lpfn_hook = 0xA1A2_A3A4_A5A6_A7A8;
            ofn.lp_template_name = 0xB1B2_B3B4_B5B6_B7B8;
            ofn.pv_reserved = 0xC1C2_C3C4_C5C6_C7C8;
            ofn.dw_reserved = 0xD1D2_D3D4;
            ofn.flags_ex = 0xE1E2_E3E4;
            Ok(())
        })
        .expect("typed OPENFILENAME write");
        let bytes = raw_bytes(&mut engine, va, 152);
        assert_eq!(&bytes[0..4], &152_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x2122_2324_2526_2728_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x3132_3334_3536_3738_u64.to_le_bytes());
        assert_eq!(
            &bytes[40..44],
            &0x1111_2222_u32.to_le_bytes(),
            "nMaxCustFilter"
        );
        assert_eq!(&bytes[44..48], &1_u32.to_le_bytes(), "nFilterIndex");
        assert_eq!(&bytes[48..56], &0x4142_4344_4546_4748_u64.to_le_bytes());
        assert_eq!(&bytes[56..60], &260_u32.to_le_bytes(), "nMaxFile");
        assert_eq!(&bytes[60..64], &[0; 4], "nMaxFile→lpstrFileTitle pad");
        assert_eq!(&bytes[64..72], &0x5152_5354_5556_5758_u64.to_le_bytes());
        assert_eq!(&bytes[72..76], &64_u32.to_le_bytes(), "nMaxFileTitle");
        assert_eq!(&bytes[76..80], &[0; 4], "nMaxFileTitle→initialDir pad");
        assert_eq!(&bytes[80..88], &0x6162_6364_6566_6768_u64.to_le_bytes());
        assert_eq!(&bytes[88..96], &0x7172_7374_7576_7778_u64.to_le_bytes());
        assert_eq!(&bytes[96..100], &0x0000_0008_u32.to_le_bytes(), "Flags");
        assert_eq!(&bytes[100..102], &9_u16.to_le_bytes(), "nFileOffset");
        assert_eq!(&bytes[102..104], &15_u16.to_le_bytes(), "nFileExtension");
        assert_eq!(&bytes[104..112], &0x8182_8384_8586_8788_u64.to_le_bytes());
        assert_eq!(&bytes[112..120], &0x9192_9394_9596_9798_u64.to_le_bytes());
        assert_eq!(&bytes[120..128], &0xA1A2_A3A4_A5A6_A7A8_u64.to_le_bytes());
        assert_eq!(&bytes[128..136], &0xB1B2_B3B4_B5B6_B7B8_u64.to_le_bytes());
        assert_eq!(
            &bytes[136..144],
            &0xC1C2_C3C4_C5C6_C7C8_u64.to_le_bytes(),
            "pvReserved"
        );
        assert_eq!(
            &bytes[144..148],
            &0xD1D2_D3D4_u32.to_le_bytes(),
            "dwReserved"
        );
        assert_eq!(&bytes[148..152], &0xE1E2_E3E4_u32.to_le_bytes(), "FlagsEx");

        with_typed_read::<OpenFileName, _, _>(&mut engine, va, |ofn| {
            assert_eq!(ofn.l_struct_size, 152);
            assert_eq!(ofn.hwnd_owner, 0x0102_0304_0506_0708);
            assert_eq!(ofn.h_instance, 0x1112_1314_1516_1718);
            assert_eq!(ofn.lpstr_filter, 0x2122_2324_2526_2728);
            assert_eq!(ofn.lpstr_custom_filter, 0x3132_3334_3536_3738);
            assert_eq!(ofn.n_max_cust_filter, 0x1111_2222);
            assert_eq!(ofn.n_filter_index, 1);
            assert_eq!(ofn.lpstr_file, 0x4142_4344_4546_4748);
            assert_eq!(ofn.n_max_file, 260);
            assert_eq!(ofn.lpstr_file_title, 0x5152_5354_5556_5758);
            assert_eq!(ofn.n_max_file_title, 64);
            assert_eq!(ofn.lpstr_initial_dir, 0x6162_6364_6566_6768);
            assert_eq!(ofn.lpstr_title, 0x7172_7374_7576_7778);
            assert_eq!(ofn.flags, 0x0000_0008);
            assert_eq!(ofn.n_file_offset, 9);
            assert_eq!(ofn.n_file_extension, 15);
            assert_eq!(ofn.lpstr_def_ext, 0x8182_8384_8586_8788);
            assert_eq!(ofn.l_cust_data, 0x9192_9394_9596_9798);
            assert_eq!(ofn.lpfn_hook, 0xA1A2_A3A4_A5A6_A7A8);
            assert_eq!(ofn.lp_template_name, 0xB1B2_B3B4_B5B6_B7B8);
            assert_eq!(ofn.pv_reserved, 0xC1C2_C3C4_C5C6_C7C8);
            assert_eq!(ofn.dw_reserved, 0xD1D2_D3D4);
            assert_eq!(ofn.flags_ex, 0xE1E2_E3E4);
            Ok(())
        })
        .expect("typed OPENFILENAME read");
    }

    #[test]
    fn find_replace_round_trip_pins_word_and_pointer_offsets() {
        let mut engine = test_engine();
        let va = 0xA200_u64;
        with_typed_write::<FindReplace, _, _>(&mut engine, va, |fr| {
            fr.l_struct_size = 80;
            fr.hwnd_owner = 0x0102_0304_0506_0708;
            fr.h_instance = 0x1112_1314_1516_1718;
            fr.flags = 0x1 | 0x4 | 0x40;
            fr.lpstr_find_what = 0x2122_2324_2526_2728;
            fr.lpstr_replace_with = 0x3132_3334_3536_3738;
            fr.w_find_what_len = 260;
            fr.w_replace_with_len = 260;
            fr.l_cust_data = 0x4142_4344_4546_4748;
            fr.lpfn_hook = 0x5152_5354_5556_5758;
            fr.lp_template_name = 0x6162_6364_6566_6768;
            Ok(())
        })
        .expect("typed FINDREPLACE write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(
            &bytes[24..28],
            &(0x1_u32 | 0x4 | 0x40).to_le_bytes(),
            "Flags"
        );
        assert_eq!(&bytes[28..32], &[0; 4], "Flags→lpstrFindWhat pad");
        assert_eq!(&bytes[32..40], &0x2122_2324_2526_2728_u64.to_le_bytes());
        assert_eq!(&bytes[40..48], &0x3132_3334_3536_3738_u64.to_le_bytes());
        assert_eq!(&bytes[48..50], &260_u16.to_le_bytes(), "wFindWhatLen");
        assert_eq!(&bytes[50..52], &260_u16.to_le_bytes(), "wReplaceWithLen");
        assert_eq!(&bytes[52..56], &[0; 4], "lens→lCustData pad");
        assert_eq!(&bytes[56..64], &0x4142_4344_4546_4748_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0x5152_5354_5556_5758_u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &0x6162_6364_6566_6768_u64.to_le_bytes());

        with_typed_read::<FindReplace, _, _>(&mut engine, va, |fr| {
            assert_eq!(fr.l_struct_size, 80);
            assert_eq!(fr.hwnd_owner, 0x0102_0304_0506_0708);
            assert_eq!(fr.h_instance, 0x1112_1314_1516_1718);
            assert_eq!(fr.flags, 0x1 | 0x4 | 0x40);
            assert_eq!(fr.lpstr_find_what, 0x2122_2324_2526_2728);
            assert_eq!(fr.lpstr_replace_with, 0x3132_3334_3536_3738);
            assert_eq!(fr.w_find_what_len, 260);
            assert_eq!(fr.w_replace_with_len, 260);
            assert_eq!(fr.l_cust_data, 0x4142_4344_4546_4748);
            assert_eq!(fr.lpfn_hook, 0x5152_5354_5556_5758);
            assert_eq!(fr.lp_template_name, 0x6162_6364_6566_6768);
            Ok(())
        })
        .expect("typed FINDREPLACE read");
    }

    /// The `write_selected_path` pattern: snapshot → edit → whole-struct
    /// write-back must leave every other field (and pad) byte-identical —
    /// the OPENFILENAME write-back must not clobber the guest's untouched
    /// fields or the reserved tail.
    #[test]
    fn open_file_name_read_modify_write_preserves_untouched_fields() {
        let mut engine = test_engine();
        let va = 0xA300_u64;
        // Seed the guest struct with raw bytes, including nonzero pads and a
        // nonzero reserved tail.
        let mut bytes = vec![0_u8; 152];
        bytes[0..4].copy_from_slice(&152_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x00AA_0001_u64.to_le_bytes()); // hwndOwner
        bytes[48..56].copy_from_slice(&0x6000_u64.to_le_bytes()); // lpstrFile
        bytes[56..60].copy_from_slice(&260_u32.to_le_bytes()); // nMaxFile
        bytes[60..64].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // pad (nonzero)
        bytes[96..100].copy_from_slice(&0x0000_0008_u32.to_le_bytes()); // Flags
        bytes[136..144].copy_from_slice(&0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes());
        engine
            .mem_write(va, &bytes)
            .expect("write raw OPENFILENAME");

        let mut ofn = read_typed_copy::<OpenFileName>(&mut engine, va).expect("typed read");
        ofn.n_file_offset = 9;
        ofn.n_file_extension = 15;
        write_typed_copy(&mut engine, va, ofn).expect("typed write-back");
        let after = raw_bytes(&mut engine, va, 152);
        assert_eq!(&after[0..4], &152_u32.to_le_bytes());
        assert_eq!(&after[8..16], &0x00AA_0001_u64.to_le_bytes());
        assert_eq!(&after[60..64], &[0xAA, 0xBB, 0xCC, 0xDD], "pad preserved");
        assert_eq!(
            &after[96..100],
            &0x0000_0008_u32.to_le_bytes(),
            "Flags preserved"
        );
        assert_eq!(&after[100..102], &9_u16.to_le_bytes(), "nFileOffset");
        assert_eq!(&after[102..104], &15_u16.to_le_bytes(), "nFileExtension");
        assert_eq!(
            &after[136..144],
            &0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes(),
            "pvReserved preserved"
        );
    }
}
