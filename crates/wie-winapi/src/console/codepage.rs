//! Console code-page conversion.
//!
//! Distinct from [`crate::vfs::encoding`], which handles the *path* boundary
//! and treats CP 437 as Latin-1. That shortcut is wrong for a console: bytes
//! `0xB0..=0xDF` in CP 437 are the shading and box-drawing glyphs, which is

#![allow(unreachable_pub)]
//! precisely what a text-mode program draws its frames with. Rendering those
//! as Latin-1 accented letters turns a game's borders into mojibake, so the
//! console keeps its own faithful table.

use super::{CP_OEM_437, CP_UTF8, CP_WINDOWS_1252};

/// Unicode scalars for CP 437 bytes `0x80..=0xFF`.
const CP437_HIGH: [u16; 128] = [
    0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, // 0x80
    0x00EA, 0x00EB, 0x00E8, 0x00EF, 0x00EE, 0x00EC, 0x00C4, 0x00C5, // 0x88
    0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9, // 0x90
    0x00FF, 0x00D6, 0x00DC, 0x00A2, 0x00A3, 0x00A5, 0x20A7, 0x0192, // 0x98
    0x00E1, 0x00ED, 0x00F3, 0x00FA, 0x00F1, 0x00D1, 0x00AA, 0x00BA, // 0xA0
    0x00BF, 0x2310, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB, // 0xA8
    0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, // 0xB0
    0x2555, 0x2563, 0x2551, 0x2557, 0x255D, 0x255C, 0x255B, 0x2510, // 0xB8
    0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x255E, 0x255F, // 0xC0
    0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, // 0xC8
    0x2568, 0x2564, 0x2565, 0x2559, 0x2558, 0x2552, 0x2553, 0x256B, // 0xD0
    0x256A, 0x2518, 0x250C, 0x2588, 0x2584, 0x258C, 0x2590, 0x2580, // 0xD8
    0x03B1, 0x00DF, 0x0393, 0x03C0, 0x03A3, 0x03C3, 0x00B5, 0x03C4, // 0xE0
    0x03A6, 0x0398, 0x03A9, 0x03B4, 0x221E, 0x03C6, 0x03B5, 0x2229, // 0xE8
    0x2261, 0x00B1, 0x2265, 0x2264, 0x2320, 0x2321, 0x00F7, 0x2248, // 0xF0
    0x00B0, 0x2219, 0x00B7, 0x221A, 0x207F, 0x00B2, 0x25A0, 0x00A0, // 0xF8
];

/// Decode one CP 437 byte to a UTF-16 code unit.
#[must_use]
pub fn cp437_byte_to_unit(byte: u8) -> u16 {
    if byte < 0x80 {
        return u16::from(byte);
    }
    let index = usize::from(byte).saturating_sub(0x80);
    CP437_HIGH.get(index).copied().unwrap_or(u16::from(byte))
}

/// Encode a UTF-16 code unit back to CP 437, or `None` if unmappable.
#[must_use]
pub fn unit_to_cp437_byte(unit: u16) -> Option<u8> {
    if unit < 0x80 {
        return u8::try_from(unit).ok();
    }
    let position = CP437_HIGH.iter().position(|&mapped| mapped == unit)?;
    u8::try_from(position.saturating_add(0x80)).ok()
}

/// Decode guest console bytes in `code_page` to UTF-16 code units.
#[must_use]
pub fn decode_to_units(code_page: u32, bytes: &[u8]) -> Vec<u16> {
    match code_page {
        CP_OEM_437 => bytes.iter().map(|&b| cp437_byte_to_unit(b)).collect(),
        CP_WINDOWS_1252 => bytes
            .iter()
            .map(|&b| crate::vfs::encoding::cp1252_byte_to_u16(b))
            .collect(),
        CP_UTF8 => match std::str::from_utf8(bytes) {
            Ok(text) => text.encode_utf16().collect(),
            Err(_) => String::from_utf8_lossy(bytes).encode_utf16().collect(),
        },
        // Unknown page: treat as UTF-8, matching `wide_to_multibyte`'s fallback.
        _ => String::from_utf8_lossy(bytes).encode_utf16().collect(),
    }
}

/// Encode UTF-16 code units to guest console bytes in `code_page`.
///
/// Unmappable characters become `?`, as Windows does with no default-char
/// override supplied.
#[must_use]
pub fn encode_from_units(code_page: u32, units: &[u16]) -> Vec<u8> {
    match code_page {
        CP_OEM_437 => units
            .iter()
            .map(|&unit| unit_to_cp437_byte(unit).unwrap_or(b'?'))
            .collect(),
        CP_WINDOWS_1252 => String::from_utf16_lossy(units)
            .chars()
            .map(|ch| crate::vfs::encoding::unicode_to_cp1252_byte(ch).unwrap_or(b'?'))
            .collect(),
        _ => String::from_utf16_lossy(units).into_bytes(),
    }
}

/// Render UTF-16 code units as the UTF-8 the host terminal expects.
#[must_use]
pub fn units_to_host_utf8(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp437_box_drawing_survives_a_round_trip() {
        // 0xC9/0xCD/0xBB are the double-line top-left, horizontal, top-right
        // corner glyphs — the corner of every text-mode dialog box ever drawn.
        for byte in [0xC9_u8, 0xCD, 0xBB, 0xDB, 0xB0] {
            let unit = cp437_byte_to_unit(byte);
            assert_eq!(unit_to_cp437_byte(unit), Some(byte));
        }
        assert_eq!(cp437_byte_to_unit(0xC9), 0x2554);
        assert_eq!(cp437_byte_to_unit(0xDB), 0x2588);
    }

    #[test]
    fn cp437_ascii_half_is_identity() {
        for byte in 0_u8..0x80 {
            assert_eq!(cp437_byte_to_unit(byte), u16::from(byte));
            assert_eq!(unit_to_cp437_byte(u16::from(byte)), Some(byte));
        }
    }

    #[test]
    fn unmappable_unit_becomes_question_mark() {
        // CJK has no CP 437 representation.
        assert_eq!(encode_from_units(CP_OEM_437, &[0x4E2D]), vec![b'?']);
    }

    #[test]
    fn utf8_page_round_trips_non_latin_text() {
        let units: Vec<u16> = "héllo".encode_utf16().collect();
        let bytes = encode_from_units(CP_UTF8, &units);
        assert_eq!(decode_to_units(CP_UTF8, &bytes), units);
    }
}
