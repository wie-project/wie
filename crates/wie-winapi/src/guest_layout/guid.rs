//! The Win32 `GUID` wire layout and its canonical text form.
//!
//! Every GUID that crosses the guest boundary or a CLSID string round-trip
//! decodes through this one type, so the memory layout and the
//! `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` rendering live in exactly one
//! place (previously `ole32` hand-packed both directions).

use core::fmt;
use zerocopy::byteorder::{LittleEndian, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Win32 `GUID` (guiddef.h): `DWORD Data1`, `WORD Data2`, `WORD Data3`,
/// `BYTE Data4[8]` — 16 bytes, align 4.
///
/// Data1/Data2/Data3 are little-endian in memory but render as big-endian
/// hex in the canonical string; Data4 is plain byte order. `Unaligned` lets
/// a `Guid` be cast straight out of guest memory at any VA — guests pass
/// GUID pointers with no alignment contract.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, KnownLayout, Immutable, FromBytes, IntoBytes, Unaligned,
)]
#[repr(C)]
pub(crate) struct Guid {
    /// `Data1` — rendered as 8 uppercase hex digits.
    pub(crate) data1: U32<LittleEndian>,
    /// `Data2` — rendered as 4 uppercase hex digits.
    pub(crate) data2: U16<LittleEndian>,
    /// `Data3` — rendered as 4 uppercase hex digits.
    pub(crate) data3: U16<LittleEndian>,
    /// `Data4` — 8 bytes, rendered in byte order (the last two hyphen groups).
    pub(crate) data4: [u8; 8],
}

/// Compile-time layout check for [`Guid`]: any field reorder or width drift
/// breaks the build instead of corrupting every COM conversation.
const _: () = {
    assert!(core::mem::size_of::<Guid>() == 16, "GUID must be 16 bytes");
    assert!(core::mem::offset_of!(Guid, data1) == 0, "GUID.Data1 @0");
    assert!(core::mem::offset_of!(Guid, data2) == 4, "GUID.Data2 @4");
    assert!(core::mem::offset_of!(Guid, data3) == 6, "GUID.Data3 @6");
    assert!(core::mem::offset_of!(Guid, data4) == 8, "GUID.Data4 @8");
};

impl Guid {
    /// Build from the raw 16-byte memory image.
    ///
    /// `Guid` is `Unaligned` with all-scalar fields and no padding, so every
    /// 16-byte pattern is valid and this cannot fail.
    #[must_use]
    pub(crate) fn from_bytes(bytes: [u8; 16]) -> Self {
        match Self::ref_from_bytes(&bytes) {
            Ok(guid) => *guid,
            Err(_) => unreachable!("every 16-byte pattern is a valid GUID"),
        }
    }

    /// The raw 16-byte memory image.
    #[must_use]
    pub(crate) fn to_bytes(self) -> [u8; 16] {
        let mut out = [0_u8; 16];
        out.copy_from_slice(self.as_bytes());
        out
    }

    /// Parse the canonical Windows form `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`
    /// (uppercase or lowercase hex, braces required).
    ///
    /// Data1/Data2/Data3 appear in the string as big-endian hex of the u32/u16
    /// values but are stored little-endian in memory; Data4 is byte order.
    pub(crate) fn parse_windows(s: &str) -> Option<Self> {
        let hex = s.trim().strip_prefix('{')?.strip_suffix('}')?;
        let parts: Vec<&str> = hex.split('-').collect();
        if parts.len() != 5 {
            return None;
        }
        let [g1, g2, g3, g4, g5] = parts.try_into().ok()?;
        if g1.len() != 8 || g2.len() != 4 || g3.len() != 4 || g4.len() != 4 || g5.len() != 12 {
            return None;
        }
        let d1 = u32::from_str_radix(g1, 16).ok()?;
        let d2 = u16::from_str_radix(g2, 16).ok()?;
        let d3 = u16::from_str_radix(g3, 16).ok()?;
        // Nibble pairs instead of from_str_radix on byte slices: radix parsing
        // accepts a leading sign (`+F`), which is not valid GUID syntax.
        fn nibbles(group: &str) -> Option<[u8; 6]> {
            let bytes = group.as_bytes();
            if !bytes.len().is_multiple_of(2) {
                return None;
            }
            let mut out = [0_u8; 6];
            for (i, pair) in bytes.chunks(2).enumerate() {
                let hi = hex_nibble(pair.first().copied()?)?;
                let lo = hex_nibble(pair.get(1).copied()?)?;
                out[i] = (hi << 4) | lo;
            }
            Some(out)
        }
        let d4_hi = nibbles(g4)?; // Data4[0..2] — two bytes
        let d4_lo = nibbles(g5)?; // Data4[2..8] — six bytes
        Some(Self {
            data1: U32::new(d1),
            data2: U16::new(d2),
            data3: U16::new(d3),
            data4: [
                d4_hi[0], d4_hi[1], d4_lo[0], d4_lo[1], d4_lo[2], d4_lo[3], d4_lo[4], d4_lo[5],
            ],
        })
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The canonical Windows rendering (`StringFromCLSID` form).
impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1.get(),
            self.data2.get(),
            self.data3.get(),
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IID_IUNKNOWN_STR: &str = "{00000000-0000-0000-C000-000000000046}";
    const IID_IUNKNOWN_BYTES: [u8; 16] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x46,
    ];

    #[test]
    fn display_matches_stringfromclsid_form() {
        let guid = Guid::from_bytes(IID_IUNKNOWN_BYTES);
        assert_eq!(guid.to_string(), IID_IUNKNOWN_STR);
    }

    #[test]
    fn parse_round_trips_display() {
        let guid = Guid::parse_windows(IID_IUNKNOWN_STR).expect("parses");
        assert_eq!(guid.to_bytes(), IID_IUNKNOWN_BYTES);
        assert_eq!(guid.to_string(), IID_IUNKNOWN_STR);
    }

    #[test]
    fn parse_accepts_lowercase_and_rejects_malformed() {
        let lower = Guid::parse_windows("{b64bb1b5-fd70-4df6-bf91-19d0a1249efc}");
        assert!(lower.is_some(), "lowercase hex must parse");
        assert!(Guid::parse_windows("00000000-0000-0000-C000-000000000046").is_none());
        assert!(Guid::parse_windows("{00000000-0000-0000-C000}").is_none());
        assert!(
            Guid::parse_windows("{00000000-0000-0000-C000-00000000004+}").is_none(),
            "sign characters are not GUID syntax"
        );
    }

    #[test]
    fn wire_layout_is_little_endian_fields_then_bytes() {
        let guid = Guid::parse_windows("{B64BB1B5-FD70-4DF6-BF91-19D0A1249EFC}").expect("parses");
        assert_eq!(
            guid.to_bytes(),
            [
                0xB5, 0xB1, 0x4B, 0xB6, // Data1 little-endian
                0x70, 0xFD, // Data2 little-endian
                0xF6, 0x4D, // Data3 little-endian
                0xBF, 0x91, 0x19, 0xD0, 0xA1, 0x24, 0x9E, 0xFC, // Data4 byte order
            ]
        );
    }
}
