//! `RT_ACCELERATOR` table parsing.
//!
//! Layout (winuser.h `ACCEL`, as stored in the resource): a sequence of
//! 6-byte entries, each `WORD fFlags`, `WORD wAnsi`, `WORD wId` — little
//! endian, no count, no terminator.

use crate::PeSectionMap;

use super::common::{RT_ACCELERATOR, parse_resource_type, read_u16_at};

/// Byte size of one `ACCEL` entry (three `WORD`s: `fFlags`, `wAnsi`, `wId`).
const ACCEL_ENTRY_SIZE: usize = 6;

/// One parsed `RT_ACCELERATOR` table.
///
/// Layout (winuser.h `ACCEL`, as stored in the resource): a sequence of
/// 6-byte entries, each `WORD fFlags`, `WORD wAnsi`, `WORD wId` — little
/// endian, no count, no terminator. `LoadAcceleratorsW` resolves a table by
/// id; `TranslateAccelerator` walks these entries per keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccelTemplate {
    /// Table id (the resource name at the second directory level); named
    /// resources have no numeric id and come back as `0` (unaddressable by
    /// `LoadAcceleratorsW`, which resolves ids only)
    pub id: u32,
    /// Language id of the table (the language-level directory entry key,
    /// e.g. `0x0409` en-US); `0` when the resource tree has no language level.
    pub lang: u16,
    /// Entries in resource order.
    pub entries: Vec<AccelEntry>,
}

/// One parsed `ACCEL` resource entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccelEntry {
    /// Raw `fFlags` bits (`FVIRTKEY`/`FSHIFT`/`FCONTROL`/`FALT`/`FNOINVERT`).
    pub flags: u16,
    /// The `wAnsi` field: a VK code when `FVIRTKEY` is set, else an ANSI char.
    pub key: u16,
    /// The `wId` field: command id posted as `WM_COMMAND` on a match.
    pub command_id: u16,
}

/// Parse every `RT_ACCELERATOR` table in `image`.
pub fn parse_accelerators(image: &[u8], sections: &[PeSectionMap]) -> Vec<AccelTemplate> {
    parse_resource_type(image, sections, RT_ACCELERATOR, parse_accel_table)
}

/// Parse one `RT_ACCELERATOR` table from its resource bytes.
///
/// Layout: 6 bytes per entry (`WORD fFlags`, `WORD wAnsi`, `WORD wId`), no
/// count and no terminator. A trailing partial entry (fewer than 6 bytes) is
/// dropped; a table with no full entry at all is treated as absent — the same
/// lenient contract as the other resource parsers.
fn parse_accel_table(table_id: u16, lang: u16, bytes: &[u8]) -> Option<AccelTemplate> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    // A partial trailing entry (fewer than 6 bytes) ends the walk; the table
    // has no count field.
    while let Some(entry) = bytes.get(pos..pos.checked_add(ACCEL_ENTRY_SIZE)?) {
        // `entry` is exactly `ACCEL_ENTRY_SIZE` bytes, so every field read
        // below succeeds.
        let flags = read_u16_at(entry, 0)?;
        let key = read_u16_at(entry, 2)?;
        let command_id = read_u16_at(entry, 4)?;
        entries.push(AccelEntry {
            flags,
            key,
            command_id,
        });
        pos = pos.checked_add(ACCEL_ENTRY_SIZE)?;
    }
    // A table that contains no full entry is treated as absent.
    if entries.is_empty() {
        return None;
    }
    Some(AccelTemplate {
        id: u32::from(table_id),
        lang,
        entries,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::common::test_util::*;
    use super::*;

    /// Emit one `ACCEL` resource entry: `WORD fFlags`, `WORD wAnsi`, `WORD wId`.
    fn accel_entry(flags: u16, key: u16, id: u16) -> Vec<u8> {
        let mut b = Vec::new();
        put_u16(&mut b, flags);
        put_u16(&mut b, key);
        put_u16(&mut b, id);
        b
    }

    #[test]
    fn parses_accel_table_entries() {
        // Notepad-style table: VIRTKEY/FCONTROL Ctrl+N, VIRTKEY/FSHIFT Shift+O,
        // VIRTKEY/FALT Alt+F, a plain-char 'a' entry, and a bare VIRTKEY F1.
        let mut b = Vec::new();
        b.extend(accel_entry(0x0009, 0x4E, 0x0100)); // FVIRTKEY|FCONTROL, VK_N
        b.extend(accel_entry(0x0005, 0x4F, 0x0103)); // FVIRTKEY|FSHIFT, VK_O
        b.extend(accel_entry(0x0011, 0x46, 0x0110)); // FVIRTKEY|FALT, VK_F
        b.extend(accel_entry(0x0000, 0x61, 0x0111)); // plain char 'a'
        b.extend(accel_entry(0x0001, 0x70, 0x0120)); // FVIRTKEY, VK_F1

        let t = parse_accel_table(0x100, 0x0409, &b).expect("table");
        assert_eq!(t.id, 0x100);
        assert_eq!(t.entries.len(), 5);
        assert_eq!(t.entries[0].flags, 0x0009);
        assert_eq!(t.entries[0].key, 0x4E);
        assert_eq!(t.entries[0].command_id, 0x0100);
        assert_eq!(t.entries[1].flags, 0x0005);
        assert_eq!(t.entries[1].key, 0x4F);
        assert_eq!(t.entries[1].command_id, 0x0103);
        assert_eq!(t.entries[2].flags, 0x0011);
        assert_eq!(t.entries[2].key, 0x46);
        assert_eq!(t.entries[3].flags, 0x0000);
        assert_eq!(t.entries[3].key, 0x61);
        assert_eq!(t.entries[3].command_id, 0x0111);
        assert_eq!(t.entries[4].flags, 0x0001);
        assert_eq!(t.entries[4].key, 0x70);
        assert_eq!(t.entries[4].command_id, 0x0120);
    }

    #[test]
    fn truncated_accel_table_is_lenient() {
        // A trailing partial entry (fewer than 6 bytes) is dropped, not fatal.
        let mut b = Vec::new();
        b.extend(accel_entry(0x0001, 0x4E, 0x0100));
        put_u16(&mut b, 0x0009);
        put_u16(&mut b, 0x4E);
        let t = parse_accel_table(1, 0x0409, &b).expect("table");
        assert_eq!(t.entries.len(), 1);
        // No bytes at all: the table is treated as absent.
        assert!(parse_accel_table(1, 0x0409, &[]).is_none());
    }

    #[test]
    fn parses_accelerators_from_synthetic_resource_tree() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1200];

        // Root dir @0x200: type RT_ACCELERATOR → subdir 0x210.
        let mut root = Vec::new();
        put_u32(&mut root, 0);
        put_u32(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 1);
        put_u32(&mut root, u32::from(RT_ACCELERATOR));
        put_u32(&mut root, 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: table id 0x100 → subdir 0x230.
        let mut type_dir = Vec::new();
        push_dir_entry(&mut type_dir, 0x0100, 0x8000_0030);
        copy_into(&mut image, 0x218, &type_dir);

        // Language dir @0x230: language 0x409 → data entry 0x248.
        let mut lang = Vec::new();
        push_dir_entry(&mut lang, 0x0409, 0x48);
        copy_into(&mut image, 0x230, &lang);

        // Data entry @0x248: table at rva 0x1200 → file 0x400.
        let mut data = Vec::new();
        put_u32(&mut data, 0x1200);
        put_u32(&mut data, 0); // patched to the real size below
        put_u32(&mut data, 0); // code page
        put_u32(&mut data, 0); // reserved
        copy_into(&mut image, 0x248, &data);

        // Table @0x400: Ctrl+N (File New) and Shift+F4 (Go To...).
        let mut tpl = Vec::new();
        tpl.extend(accel_entry(0x0009, 0x4E, 0x0100));
        tpl.extend(accel_entry(0x0005, 0x74, 0x0123));
        copy_into(&mut image, 0x400, &tpl);
        let size_bytes = u32::try_from(tpl.len()).expect("size fits");
        copy_into(&mut image, 0x24C, &size_bytes.to_le_bytes());

        let tables = parse_accelerators(&image, &sections);
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.id, 0x100);
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0].flags, 0x0009);
        assert_eq!(t.entries[0].key, 0x4E);
        assert_eq!(t.entries[0].command_id, 0x0100);
        assert_eq!(t.entries[1].key, 0x74);
        assert_eq!(t.entries[1].command_id, 0x0123);
    }

    #[test]
    fn no_accelerator_resource_yields_empty() {
        assert!(parse_accelerators(&[], &[]).is_empty());
        assert!(parse_accelerators(&[0_u8; 64], &[]).is_empty());
    }
}
