//! kernel32 lane: `FindDataHeader` plus the `WIN32_FIND_DATA` name
//! offsets, `StartupInfo`, `ByHandleFileInformation`, `FileAttributeData`,
//! `SystemTime`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// --- kernel32 lane: WIN32_FIND_DATA header / STARTUPINFO / BY_HANDLE_FILE_INFORMATION / ---
// --- WIN32_FILE_ATTRIBUTE_DATA / SYSTEMTIME (sizes verified against mingw-w64 14.0.0) ---

/// Win64 `WIN32_FIND_DATA{A,W}` common header (minwinbase.h): `DWORD
/// dwFileAttributes` @0, `FILETIME ftCreationTime` @4 (`dwLowDateTime` @4,
/// `dwHighDateTime` @8), `ftLastAccessTime` @12, `ftLastWriteTime` @20,
/// `nFileSizeHigh` @28, `nFileSizeLow` @32, `dwReserved0` @36, `dwReserved1`
/// @40 — 44 bytes, align 4.
///
/// The A and W variants share this header exactly; they differ only in the
/// trailing name fields (`WCHAR cFileName[260]` @44 for W, `CHAR
/// cFileName[260]` @44 for A), which the callers write with the existing
/// string-write path. `FILETIME` is two `DWORD`s, not a `u64`, so the time
/// fields are split low/high to keep the struct `u32`-aligned throughout.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FindDataHeader {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
    pub(crate) dw_reserved0: u32,
    pub(crate) dw_reserved1: u32,
}

/// Offset of `cFileName` in both variants (`0x2C`).
pub(crate) const FIND_DATA_FILE_NAME_OFFSET: u64 = 44;
/// Offset of `cAlternateFileName` in the W variant (`0x234`: 44 + 260×2).
pub(crate) const FIND_DATA_W_ALT_NAME_OFFSET: u64 = 564;
/// Offset of `cAlternateFileName` in the A variant (44 + 260).
pub(crate) const FIND_DATA_A_ALT_NAME_OFFSET: u64 = 304;

/// Compile-time layout check for [`FindDataHeader`] plus the full-struct name
/// offsets. The W struct is 592 bytes (44 + 520 + 28); the A struct is 318
/// payload bytes (44 + 260 + 14) rounded to 320 by its 4-byte alignment.
const _: () = {
    assert!(
        core::mem::size_of::<FindDataHeader>() == 44,
        "WIN32_FIND_DATA header must be 44 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_creation_time_high) == 8,
        "ftCreationTime.high @8"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, n_file_size_high) == 28,
        "nFileSizeHigh @28"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, n_file_size_low) == 32,
        "nFileSizeLow @32"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_reserved0) == 36,
        "dwReserved0 @36"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_reserved1) == 40,
        "dwReserved1 @40"
    );
    assert!(FIND_DATA_FILE_NAME_OFFSET == 44, "cFileName @44 (0x2C)");
    assert!(
        FIND_DATA_W_ALT_NAME_OFFSET == 564,
        "W cAlternateFileName @564 (0x234)"
    );
    assert!(
        FIND_DATA_A_ALT_NAME_OFFSET == 304,
        "A cAlternateFileName @304"
    );
};

/// Win64 `STARTUPINFOW` / `STARTUPINFOA` (winbase.h) — layout-identical on
/// Win64 because the ANSI variant's character pointers are still 8 bytes:
/// `DWORD cb` @0, [pad @4], `LPWSTR lpReserved` @8, `lpDesktop` @16,
/// `lpTitle` @24, `DWORD dwX` @32, `dwY` @36, `dwXSize` @40, `dwYSize` @44,
/// `dwXCountChars` @48, `dwYCountChars` @52, `dwFillAttribute` @56,
/// `dwFlags` @60, `WORD wShowWindow` @64, `WORD cbReserved2` @66, [pad @68],
/// `LPBYTE lpReserved2` @72, `HANDLE hStdInput` @80, `hStdOutput` @88,
/// `hStdError` @96 — 104 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct StartupInfo {
    pub(crate) cb: u32,
    /// Win64 alignment padding between `cb` and `lpReserved`.
    pub(crate) _pad0: [u8; 4],
    pub(crate) lp_reserved: u64,
    pub(crate) lp_desktop: u64,
    pub(crate) lp_title: u64,
    pub(crate) dw_x: u32,
    pub(crate) dw_y: u32,
    pub(crate) dw_x_size: u32,
    pub(crate) dw_y_size: u32,
    pub(crate) dw_x_count_chars: u32,
    pub(crate) dw_y_count_chars: u32,
    pub(crate) dw_fill_attribute: u32,
    pub(crate) dw_flags: u32,
    pub(crate) w_show_window: u16,
    pub(crate) cb_reserved2: u16,
    /// Win64 alignment padding between `cbReserved2` and `lpReserved2`.
    pub(crate) _pad1: [u8; 4],
    pub(crate) lp_reserved2: u64,
    pub(crate) h_std_input: u64,
    pub(crate) h_std_output: u64,
    pub(crate) h_std_error: u64,
}

/// Compile-time layout check for [`StartupInfo`]: `cb = 104` matches the
/// documented `STARTUPINFOW`/`STARTUPINFOA` size.
const _: () = {
    assert!(
        core::mem::size_of::<StartupInfo>() == 104,
        "STARTUPINFO must be 104 bytes on Win64"
    );
    assert!(core::mem::offset_of!(StartupInfo, cb) == 0, "cb @0");
    assert!(
        core::mem::offset_of!(StartupInfo, lp_reserved) == 8,
        "lpReserved @8"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_desktop) == 16,
        "lpDesktop @16"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_title) == 24,
        "lpTitle @24"
    );
    assert!(core::mem::offset_of!(StartupInfo, dw_x) == 32, "dwX @32");
    assert!(core::mem::offset_of!(StartupInfo, dw_y) == 36, "dwY @36");
    assert!(
        core::mem::offset_of!(StartupInfo, dw_x_size) == 40,
        "dwXSize @40"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_y_size) == 44,
        "dwYSize @44"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_x_count_chars) == 48,
        "dwXCountChars @48"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_y_count_chars) == 52,
        "dwYCountChars @52"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_fill_attribute) == 56,
        "dwFillAttribute @56"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_flags) == 60,
        "dwFlags @60"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, w_show_window) == 64,
        "wShowWindow @64"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, cb_reserved2) == 66,
        "cbReserved2 @66"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_reserved2) == 72,
        "lpReserved2 @72"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_input) == 80,
        "hStdInput @80"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_output) == 88,
        "hStdOutput @88"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_error) == 96,
        "hStdError @96"
    );
};

/// Win64 `BY_HANDLE_FILE_INFORMATION` (fileapi.h, `GetFileInformationByHandle`):
/// `DWORD dwFileAttributes` @0, `FILETIME ftCreationTime` @4,
/// `ftLastAccessTime` @12, `ftLastWriteTime` @20, `DWORD dwVolumeSerialNumber`
/// @28, `nFileSizeHigh` @32, `nFileSizeLow` @36, `nNumberOfLinks` @40,
/// `nFileIndexHigh` @44, `nFileIndexLow` @48 — 52 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct ByHandleFileInformation {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) dw_volume_serial_number: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
    pub(crate) n_number_of_links: u32,
    pub(crate) n_file_index_high: u32,
    pub(crate) n_file_index_low: u32,
}

/// Compile-time layout check for [`ByHandleFileInformation`].
const _: () = {
    assert!(
        core::mem::size_of::<ByHandleFileInformation>() == 52,
        "BY_HANDLE_FILE_INFORMATION must be 52 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, dw_volume_serial_number) == 28,
        "dwVolumeSerialNumber @28"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_size_high) == 32,
        "nFileSizeHigh @32"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_size_low) == 36,
        "nFileSizeLow @36"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_number_of_links) == 40,
        "nNumberOfLinks @40"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_index_high) == 44,
        "nFileIndexHigh @44"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_index_low) == 48,
        "nFileIndexLow @48"
    );
};

/// Win64 `WIN32_FILE_ATTRIBUTE_DATA` (fileapi.h, `GetFileAttributesEx`):
/// `DWORD dwFileAttributes` @0, `FILETIME ftCreationTime` @4,
/// `ftLastAccessTime` @12, `ftLastWriteTime` @20, `nFileSizeHigh` @28,
/// `nFileSizeLow` @32 — 36 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FileAttributeData {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
}

/// Compile-time layout check for [`FileAttributeData`].
const _: () = {
    assert!(
        core::mem::size_of::<FileAttributeData>() == 36,
        "WIN32_FILE_ATTRIBUTE_DATA must be 36 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, n_file_size_high) == 28,
        "nFileSizeHigh @28"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, n_file_size_low) == 32,
        "nFileSizeLow @32"
    );
};

/// Win64 `SYSTEMTIME` (minwinbase.h): eight `WORD` fields, `wYear` @0 through
/// `wMilliseconds` @14 — 16 bytes, align 2.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct SystemTime {
    pub(crate) w_year: u16,
    pub(crate) w_month: u16,
    pub(crate) w_day_of_week: u16,
    pub(crate) w_day: u16,
    pub(crate) w_hour: u16,
    pub(crate) w_minute: u16,
    pub(crate) w_second: u16,
    pub(crate) w_milliseconds: u16,
}

/// Compile-time layout check for [`SystemTime`].
const _: () = {
    assert!(
        core::mem::size_of::<SystemTime>() == 16,
        "SYSTEMTIME must be 16 bytes on Win64"
    );
    assert!(core::mem::offset_of!(SystemTime, w_year) == 0, "wYear @0");
    assert!(core::mem::offset_of!(SystemTime, w_month) == 2, "wMonth @2");
    assert!(
        core::mem::offset_of!(SystemTime, w_day_of_week) == 4,
        "wDayOfWeek @4"
    );
    assert!(core::mem::offset_of!(SystemTime, w_day) == 6, "wDay @6");
    assert!(core::mem::offset_of!(SystemTime, w_hour) == 8, "wHour @8");
    assert!(
        core::mem::offset_of!(SystemTime, w_minute) == 10,
        "wMinute @10"
    );
    assert!(
        core::mem::offset_of!(SystemTime, w_second) == 12,
        "wSecond @12"
    );
    assert!(
        core::mem::offset_of!(SystemTime, w_milliseconds) == 14,
        "wMilliseconds @14"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod kernel32_lane_tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
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

    /// The `FIXED_SYSTEM_FILETIME` constant (kernel32/mod.rs) split into the
    /// (low, high) `DWORD` pair the layouts carry.
    fn fixed_filetime_parts() -> (u32, u32) {
        const FT: u64 = 133_485_408_000_000_000;
        (
            u32::try_from(FT & 0xffff_ffff).unwrap_or(0),
            u32::try_from(FT >> 32).unwrap_or(0),
        )
    }

    #[test]
    fn find_data_header_write_zero_fills_reserved_fields() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<FindDataHeader, _, _>(&mut engine, 0x9000, |header| {
            header.dw_file_attributes = 0x20;
            header.ft_creation_time_low = ft_low;
            header.ft_creation_time_high = ft_high;
            header.ft_last_access_time_low = ft_low;
            header.ft_last_access_time_high = ft_high;
            header.ft_last_write_time_low = ft_low;
            header.ft_last_write_time_high = ft_high;
            header.n_file_size_high = 1;
            header.n_file_size_low = 2;
            // dwReserved0 / dwReserved1 deliberately left unset: the view
            // starts zeroed (the hand-built header zeroed them explicitly).
            Ok(())
        })
        .expect("typed header write");
        let bytes = raw_bytes(&mut engine, 0x9000, 44);
        assert_eq!(&bytes[0..4], &0x20_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &ft_low.to_le_bytes());
        assert_eq!(&bytes[8..12], &ft_high.to_le_bytes());
        assert_eq!(&bytes[12..16], &ft_low.to_le_bytes(), "access time.low");
        assert_eq!(&bytes[16..20], &ft_high.to_le_bytes(), "access time.high");
        assert_eq!(&bytes[20..24], &ft_low.to_le_bytes(), "write time.low");
        assert_eq!(&bytes[24..28], &ft_high.to_le_bytes(), "write time.high");
        assert_eq!(&bytes[28..32], &1_u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &2_u32.to_le_bytes());
        assert_eq!(&bytes[36..44], &[0; 8], "dwReserved0/1 zero-filled");
    }

    #[test]
    fn find_data_header_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&0x10_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&7_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&8_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&9_u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&11_u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&12_u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
        engine
            .mem_write(0x9000, &bytes)
            .expect("write raw header bytes");
        with_typed_read::<FindDataHeader, _, _>(&mut engine, 0x9000, |header| {
            assert_eq!(header.dw_file_attributes, 0x10);
            assert_eq!(header.ft_creation_time_low, 7);
            assert_eq!(header.ft_creation_time_high, 8);
            assert_eq!(header.ft_last_access_time_low, 9);
            assert_eq!(header.ft_last_access_time_high, 0);
            assert_eq!(header.ft_last_write_time_low, 0);
            assert_eq!(header.n_file_size_high, 11);
            assert_eq!(header.n_file_size_low, 12);
            assert_eq!(header.dw_reserved0, 0);
            assert_eq!(header.dw_reserved1, 0xDEAD_BEEF);
            Ok(())
        })
        .expect("typed header read");
    }

    #[test]
    fn find_data_w_full_round_trip_includes_names() {
        // Full 592-byte WIN32_FIND_DATAW: the 44-byte header via the typed
        // view, the name via the existing string-write path. Pre-fill with
        // 0xAA so untouched tail bytes are observable (the old code only
        // wrote the name + NUL and one alternate-name NUL, leaving the rest
        // as caller garbage — this test pins that semantics).
        let mut engine = test_engine();
        engine
            .mem_write(0x9100, &[0xAA_u8; 592])
            .expect("prefill WIN32_FIND_DATAW");
        crate::kernel32::file_io::write_find_data_w(
            &mut engine,
            0x9100,
            "test.txt",
            0x20,
            0x1_0000_0042,
        )
        .expect("write_find_data_w");
        let bytes = raw_bytes(&mut engine, 0x9100, 592);
        assert_eq!(&bytes[0..4], &0x20_u32.to_le_bytes(), "dwFileAttributes");
        let (ft_low, ft_high) = fixed_filetime_parts();
        assert_eq!(&bytes[4..8], &ft_low.to_le_bytes(), "ftCreationTime.low");
        assert_eq!(&bytes[8..12], &ft_high.to_le_bytes(), "ftCreationTime.high");
        assert_eq!(&bytes[12..20], &bytes[4..12], "access mirrors creation");
        assert_eq!(&bytes[20..28], &bytes[4..12], "write mirrors creation");
        assert_eq!(&bytes[28..32], &1_u32.to_le_bytes(), "nFileSizeHigh");
        assert_eq!(&bytes[32..36], &0x42_u32.to_le_bytes(), "nFileSizeLow");
        assert_eq!(&bytes[36..44], &[0; 8], "dwReserved0/1");
        let mut name = Vec::new();
        for unit in "test.txt".encode_utf16() {
            name.extend_from_slice(&unit.to_le_bytes());
        }
        name.extend_from_slice(&0_u16.to_le_bytes());
        assert_eq!(&bytes[44..44 + name.len()], &name, "cFileName");
        let tail_len = 564 - 44 - name.len();
        assert_eq!(
            &bytes[44 + name.len()..564],
            &vec![0xAA_u8; tail_len],
            "cFileName tail untouched (old semantics)"
        );
        assert_eq!(&bytes[564..566], &[0, 0], "cAlternateFileName NUL");
        assert_eq!(&bytes[566..592], &[0xAA_u8; 26], "alternate tail untouched");
    }

    #[test]
    fn startup_info_write_matches_get_startup_info_semantics() {
        // Mirror of the state/tests.rs GetStartupInfoW assertions: cb = 104,
        // dwFlags = 0, wShowWindow = 1, everything else zeroed.
        let mut engine = test_engine();
        engine
            .mem_write(0x9200, &[0xAA_u8; 104])
            .expect("prefill STARTUPINFO");
        with_typed_write::<StartupInfo, _, _>(&mut engine, 0x9200, |info| {
            info.cb = 104;
            info.dw_flags = 0;
            info.w_show_window = 1;
            Ok(())
        })
        .expect("typed STARTUPINFO write");
        let bytes = raw_bytes(&mut engine, 0x9200, 104);
        assert_eq!(&bytes[0..4], &104_u32.to_le_bytes());
        assert_eq!(&bytes[64..66], &1_u16.to_le_bytes());
        let mut nonzero_offsets: Vec<usize> = Vec::new();
        for (offset, &byte) in bytes.iter().enumerate() {
            if byte != 0 {
                nonzero_offsets.push(offset);
            }
        }
        assert_eq!(
            nonzero_offsets,
            vec![0, 64],
            "only cb and wShowWindow may be nonzero; the rest must be zeroed"
        );
    }

    #[test]
    fn by_handle_file_information_write_matches_hand_written_pattern() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<ByHandleFileInformation, _, _>(&mut engine, 0x9300, |info| {
            info.dw_file_attributes = 0x20;
            info.ft_creation_time_low = ft_low;
            info.ft_creation_time_high = ft_high;
            info.ft_last_access_time_low = ft_low;
            info.ft_last_access_time_high = ft_high;
            info.ft_last_write_time_low = ft_low;
            info.ft_last_write_time_high = ft_high;
            info.dw_volume_serial_number = 0x1234_abcd;
            info.n_file_size_high = 0x12;
            info.n_file_size_low = 0x3456_7890;
            info.n_number_of_links = 1;
            info.n_file_index_high = 0;
            info.n_file_index_low = 1;
            Ok(())
        })
        .expect("typed BY_HANDLE write");
        let bytes = raw_bytes(&mut engine, 0x9300, 52);
        let mut expected = vec![0_u8; 52];
        expected[0..4].copy_from_slice(&0x20_u32.to_le_bytes());
        expected[4..12].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[12..20].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[20..28].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[28..32].copy_from_slice(&0x1234_abcd_u32.to_le_bytes());
        expected[32..36].copy_from_slice(&0x12_u32.to_le_bytes());
        expected[36..40].copy_from_slice(&0x3456_7890_u32.to_le_bytes());
        expected[40..44].copy_from_slice(&1_u32.to_le_bytes());
        expected[44..48].copy_from_slice(&0_u32.to_le_bytes());
        expected[48..52].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(bytes, expected, "BY_HANDLE_FILE_INFORMATION byte drift");
    }

    #[test]
    fn file_attribute_data_write_and_read_back() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<FileAttributeData, _, _>(&mut engine, 0x9400, |data| {
            data.dw_file_attributes = 0x10;
            data.ft_creation_time_low = ft_low;
            data.ft_creation_time_high = ft_high;
            data.ft_last_access_time_low = ft_low;
            data.ft_last_access_time_high = ft_high;
            data.ft_last_write_time_low = ft_low;
            data.ft_last_write_time_high = ft_high;
            data.n_file_size_high = 0;
            data.n_file_size_low = 1234;
            Ok(())
        })
        .expect("typed WIN32_FILE_ATTRIBUTE_DATA write");
        let bytes = raw_bytes(&mut engine, 0x9400, 36);
        assert_eq!(&bytes[0..4], &0x10_u32.to_le_bytes());
        assert_eq!(&bytes[4..12], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[12..20], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[20..28], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[28..32], &0_u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &1234_u32.to_le_bytes());

        with_typed_read::<FileAttributeData, _, _>(&mut engine, 0x9400, |data| {
            assert_eq!(data.dw_file_attributes, 0x10);
            assert_eq!(data.ft_creation_time_low, ft_low);
            assert_eq!(data.ft_creation_time_high, ft_high);
            assert_eq!(data.ft_last_write_time_high, ft_high);
            assert_eq!(data.n_file_size_low, 1234);
            Ok(())
        })
        .expect("typed WIN32_FILE_ATTRIBUTE_DATA read");
    }

    #[test]
    fn system_time_write_and_misaligned_stage_match_raw() {
        let mut engine = test_engine();
        with_typed_write::<SystemTime, _, _>(&mut engine, 0x9500, |st| {
            st.w_year = 2026;
            st.w_month = 7;
            st.w_day_of_week = 4;
            st.w_day = 9;
            st.w_hour = 12;
            st.w_minute = 30;
            st.w_second = 45;
            st.w_milliseconds = 100;
            Ok(())
        })
        .expect("typed SYSTEMTIME write");
        let bytes = raw_bytes(&mut engine, 0x9500, 16);
        assert_eq!(&bytes[0..2], &2026_u16.to_le_bytes());
        assert_eq!(&bytes[2..4], &7_u16.to_le_bytes());
        assert_eq!(&bytes[4..6], &4_u16.to_le_bytes());
        assert_eq!(&bytes[6..8], &9_u16.to_le_bytes());
        assert_eq!(&bytes[8..10], &12_u16.to_le_bytes());
        assert_eq!(&bytes[10..12], &30_u16.to_le_bytes());
        assert_eq!(&bytes[12..14], &45_u16.to_le_bytes());
        assert_eq!(&bytes[14..16], &100_u16.to_le_bytes());

        // Odd guest address: the helper must stage into an aligned host
        // buffer and produce identical bytes.
        let mut staged_engine = test_engine();
        with_typed_write::<SystemTime, _, _>(&mut staged_engine, 0x9501, |st| {
            st.w_year = 2026;
            st.w_month = 7;
            st.w_day_of_week = 4;
            st.w_day = 9;
            st.w_hour = 12;
            st.w_minute = 30;
            st.w_second = 45;
            st.w_milliseconds = 100;
            Ok(())
        })
        .expect("staged SYSTEMTIME write");
        let staged = raw_bytes(&mut staged_engine, 0x9501, 16);
        assert_eq!(staged, bytes, "staged write must match aligned write");
    }
}
