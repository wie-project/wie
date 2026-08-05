//! `RT_STRING` block parsing.
//!
//! A string table stores its entries in blocks of 16 (Microsoft Learn,
//! "String Table"); each block is 16 length-prefixed UTF-16 strings.

use crate::PeSectionMap;

use super::common::{MAX_STRING_WORDS, RT_STRING, parse_resource_type, read_u16_at};

/// One parsed `RT_STRING` block: 16 length-prefixed UTF-16 strings.
///
/// A string table stores its entries in blocks of 16 (Microsoft Learn,
/// "String Table"). The resource name at the second directory level is the
/// block id, and a string's id maps as `block = (id >> 4) + 1`,
/// `slot = id & 0xF`. Block names are **1-based** because `rc.exe` never emits
/// a block named 0 (`MAKEINTRESOURCE(0)` is the null resource) — verified
/// against notepad.exe, where "Untitled" lives in the block named 24 at
/// slot 4 and is loaded by id 0x174. Each slot is a `u16` length prefix
/// followed by that many UTF-16LE units with **no** NUL terminator; a zero
/// length is the empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringBlock {
    /// Block id — equal to `(string_id >> 4) + 1` for every string in it.
    pub block: u16,
    /// Language id of the block (the language-level directory entry key,
    /// e.g. `0x0409` en-US); `0` when the resource tree has no language level.
    pub lang: u16,
    /// The 16 strings of the block, indexed by slot (`string_id & 0xF`).
    pub strings: [String; 16],
}

/// Parse every `RT_STRING` block in `image`.
pub fn parse_strings(image: &[u8], sections: &[PeSectionMap]) -> Vec<StringBlock> {
    parse_resource_type(image, sections, RT_STRING, parse_string_block)
}

/// Parse one `RT_STRING` block from its resource bytes.
///
/// Layout (Microsoft Learn): 16 slots, each a `u16` length prefix followed by
/// that many UTF-16LE units and **no** NUL terminator. A zero length is the
/// empty string. Parsing is lenient like the other resource parsers: a block
/// truncated mid-string yields empty strings for the slots that do not fit
/// (never fails the caller).
fn parse_string_block(block_id: u16, lang: u16, bytes: &[u8]) -> Option<StringBlock> {
    if bytes.len() < 2 {
        // Not even one length prefix: treat the block as absent.
        return None;
    }
    let mut strings: [String; 16] = std::array::from_fn(|_| String::new());
    let mut pos = 0usize;
    for slot in &mut strings {
        let Some(len) = read_u16_at(bytes, pos) else {
            break;
        };
        let len = usize::from(len);
        if len > MAX_STRING_WORDS {
            break;
        }
        let body_start = pos.checked_add(2)?;
        let body_end = body_start.checked_add(len.checked_mul(2)?)?;
        // Bounds-check the whole string body; every unit read below then fits.
        if bytes.get(body_start..body_end).is_none() {
            break;
        }
        let mut units = Vec::with_capacity(len);
        let mut p = body_start;
        for _ in 0..len {
            // `body_end` is bounds-checked above, so every unit read succeeds.
            units.push(read_u16_at(bytes, p)?);
            p = p.checked_add(2)?;
        }
        *slot = String::from_utf16_lossy(&units);
        pos = body_end;
    }
    Some(StringBlock {
        block: block_id,
        lang,
        strings,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::common::test_util::*;
    use super::*;

    /// Build the body of an `RT_STRING` block: 16 length-prefixed strings.
    ///
    /// `strings` fills slots from the start; the remaining slots get a zero
    /// length prefix (the empty string).
    fn string_block_body(strings: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        for s in strings {
            let units: Vec<u16> = s.encode_utf16().collect();
            put_u16(
                &mut b,
                u16::try_from(units.len()).expect("string length fits"),
            );
            for unit in units {
                put_u16(&mut b, unit);
            }
        }
        for _ in strings.len()..16 {
            put_u16(&mut b, 0);
        }
        b
    }

    #[test]
    fn parses_string_block_slots() {
        let b = string_block_body(&["Untitled", "", "café — ✓"]);
        let block = parse_string_block(0x2A, 0x0409, &b).expect("block");
        assert_eq!(block.block, 0x2A);
        assert_eq!(block.strings[0], "Untitled");
        assert_eq!(block.strings[1], "");
        assert_eq!(block.strings[2], "café — ✓");
        for slot in 3..16 {
            assert_eq!(block.strings[slot], "");
        }
    }

    #[test]
    fn empty_string_block_yields_empty_strings() {
        let b = string_block_body(&[]);
        let block = parse_string_block(7, 0x0409, &b).expect("block");
        assert_eq!(block.block, 7);
        assert!(block.strings.iter().all(|s| s.is_empty()));
    }

    #[test]
    fn truncated_string_block_is_lenient() {
        // Length prefix claims 5 units but only 2 are present: the partial
        // slot and everything after it stay empty; the block still parses.
        let mut b = Vec::new();
        put_u16(&mut b, 5);
        put_u16(&mut b, 0x0041); // 'A'
        put_u16(&mut b, 0x0042); // 'B'
        let block = parse_string_block(0, 0x0409, &b).expect("block");
        assert!(block.strings.iter().all(|s| s.is_empty()));
        // A block with no length prefix at all is treated as absent.
        assert!(parse_string_block(0, 0x0409, &[]).is_none());
    }

    #[test]
    fn string_id_maps_to_block_and_slot() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1400];

        // Root dir @0x200: type RT_STRING → type dir @0x218.
        let mut root = Vec::new();
        push_dir_entry(&mut root, u32::from(RT_STRING), 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: three blocks (1, 2, 3 — block names are 1-based in
        // real rc.exe output) → lang dirs @0x240/0x258/0x270.
        let mut type_dir = Vec::new();
        put_u32(&mut type_dir, 0); // characteristics
        put_u32(&mut type_dir, 0); // timestamp
        put_u16(&mut type_dir, 0); // major version
        put_u16(&mut type_dir, 0); // minor version
        put_u16(&mut type_dir, 0); // named entries
        put_u16(&mut type_dir, 3); // three block ids
        put_u32(&mut type_dir, 1);
        put_u32(&mut type_dir, 0x8000_0040);
        put_u32(&mut type_dir, 2);
        put_u32(&mut type_dir, 0x8000_0058);
        put_u32(&mut type_dir, 3);
        put_u32(&mut type_dir, 0x8000_0070);
        copy_into(&mut image, 0x218, &type_dir);

        // Language dirs @0x240/0x258/0x270 → data entries @0x288/0x298/0x2A8.
        let mut lang0 = Vec::new();
        push_dir_entry(&mut lang0, 0x0409, 0x88);
        copy_into(&mut image, 0x240, &lang0);
        let mut lang1 = Vec::new();
        push_dir_entry(&mut lang1, 0x0409, 0x98);
        copy_into(&mut image, 0x258, &lang1);
        let mut lang2 = Vec::new();
        push_dir_entry(&mut lang2, 0x0409, 0xA8);
        copy_into(&mut image, 0x270, &lang2);

        // Data entries: block bodies at rva 0x1200/0x1300/0x1400
        // (file 0x400/0x500/0x600).
        for (entry_off, rva) in [(0x288_usize, 0x1200_u32), (0x298, 0x1300), (0x2A8, 0x1400)] {
            let mut data = Vec::new();
            put_u32(&mut data, rva);
            put_u32(&mut data, 0); // patched to the real size below
            put_u32(&mut data, 0); // code page
            put_u32(&mut data, 0); // reserved
            copy_into(&mut image, entry_off, &data);
        }

        // Block bodies with their size fields patched into the data entries.
        let body0 = string_block_body(&["Untitled", "", "Save As..."]);
        copy_into(&mut image, 0x400, &body0);
        copy_into(
            &mut image,
            0x28C,
            &u32::try_from(body0.len()).expect("size fits").to_le_bytes(),
        );
        let body1 = string_block_body(&["first of block 1", "second"]);
        copy_into(&mut image, 0x500, &body1);
        copy_into(
            &mut image,
            0x29C,
            &u32::try_from(body1.len()).expect("size fits").to_le_bytes(),
        );
        let body2 = string_block_body(&["third block"]);
        copy_into(&mut image, 0x600, &body2);
        copy_into(
            &mut image,
            0x2AC,
            &u32::try_from(body2.len()).expect("size fits").to_le_bytes(),
        );

        let blocks = parse_strings(&image, &sections);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].block, 1);
        assert_eq!(blocks[1].block, 2);
        assert_eq!(blocks[2].block, 3);

        // Resolve the way LoadString does: block = (id >> 4) + 1, slot = id & 0xF.
        let lookup = |id: u16| -> String {
            blocks
                .iter()
                .find(|b| b.block == (id >> 4) + 1)
                .and_then(|b| b.strings.get(usize::from(id & 0xF)))
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(lookup(0x00), "Untitled");
        assert_eq!(lookup(0x01), "");
        assert_eq!(lookup(0x02), "Save As...");
        assert_eq!(lookup(0x10), "first of block 1");
        assert_eq!(lookup(0x11), "second");
        assert_eq!(lookup(0x20), "third block");
        // A slot that has no string (block 3 only fills slot 0).
        assert_eq!(lookup(0x21), "");
        // A block that was never parsed resolves to empty.
        assert_eq!(lookup(0x40), "");
    }

    #[test]
    fn no_string_resource_yields_empty() {
        assert!(parse_strings(&[], &[]).is_empty());
        assert!(parse_strings(&[0_u8; 64], &[]).is_empty());
    }

    /// One string block id with two language blocks: German 0x0007 first and
    /// en-US 0x0409 second (notepad.exe's resource-directory order). Both
    /// parsed blocks must carry their language id.
    #[test]
    fn language_leaves_carry_their_lang_id() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1400];

        // Root dir @0x200: type RT_STRING → type dir @0x218.
        let mut root = Vec::new();
        push_dir_entry(&mut root, u32::from(RT_STRING), 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: block id 1 → lang dir @0x240.
        let mut type_dir = Vec::new();
        push_dir_entry(&mut type_dir, 1, 0x8000_0040);
        copy_into(&mut image, 0x218, &type_dir);

        // Lang dir @0x240: two entries — 0x0007 German first (ascending
        // LANGID order, like rc.exe), then 0x0409 en-US. Entry offsets are
        // relative to the root dir @0x200, so 0x68/0x78 → data @0x268/0x278
        // (clear of the entries at 0x250..0x25F).
        let mut lang = Vec::new();
        put_u32(&mut lang, 0); // characteristics
        put_u32(&mut lang, 0); // timestamp
        put_u16(&mut lang, 0); // major version
        put_u16(&mut lang, 0); // minor version
        put_u16(&mut lang, 0); // named entries
        put_u16(&mut lang, 2); // two id entries
        put_u32(&mut lang, 0x0007);
        put_u32(&mut lang, 0x68);
        put_u32(&mut lang, 0x0409);
        put_u32(&mut lang, 0x78);
        copy_into(&mut image, 0x240, &lang);

        // Data entries @0x268 (German) and @0x278 (en-US); block bodies at
        // rva 0x1200 (file 0x400) and 0x1300 (file 0x500).
        for (entry_off, rva) in [(0x268_usize, 0x1200_u32), (0x278, 0x1300)] {
            let mut data = Vec::new();
            put_u32(&mut data, rva);
            put_u32(&mut data, 0); // patched to the real size below
            put_u32(&mut data, 0); // code page
            put_u32(&mut data, 0); // reserved
            copy_into(&mut image, entry_off, &data);
        }

        // Notepad-like bodies: the "Untitled" title at slot 4, the file-type
        // filter at slot 6 (ids 0x174 / 0x176 → block 24).
        let german = string_block_body(&["", "", "", "", "Unbenannt", "", "Textdateien (*.txt)"]);
        copy_into(&mut image, 0x400, &german);
        copy_into(
            &mut image,
            0x26C,
            &u32::try_from(german.len())
                .expect("size fits")
                .to_le_bytes(),
        );
        let english = string_block_body(&["", "", "", "", "Untitled", "", "Text files (*.txt)"]);
        copy_into(&mut image, 0x500, &english);
        copy_into(
            &mut image,
            0x27C,
            &u32::try_from(english.len())
                .expect("size fits")
                .to_le_bytes(),
        );

        let blocks = parse_strings(&image, &sections);
        assert_eq!(blocks.len(), 2, "both locale blocks must parse");
        // Directory order is preserved: German first.
        assert_eq!(blocks[0].lang, 0x0007, "first block is the German locale");
        assert_eq!(blocks[0].strings[4], "Unbenannt");
        assert_eq!(blocks[0].strings[6], "Textdateien (*.txt)");
        assert_eq!(blocks[1].lang, 0x0409, "second block is en-US");
        assert_eq!(blocks[1].strings[4], "Untitled");
        assert_eq!(blocks[1].strings[6], "Text files (*.txt)");
    }
}
