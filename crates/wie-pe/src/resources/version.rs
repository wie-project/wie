//! `RT_VERSION` resource parsing.
//!
//! A version resource is a `VS_VERSIONINFO` tree (Microsoft Learn, "Version
//! Information resource"): a root node keyed `VS_VERSION_INFO`, an optional
//! 52-byte `VS_FIXEDFILEINFO` value, and `StringFileInfo` / `VarFileInfo`
//! children carrying the string table and the translation list.
//!
//! Every node shares one header layout, so a single recursive walker serves
//! the whole tree: `wLength` (u16, total node length), `wValueLength` (u16),
//! `wType` (u16), a NUL-terminated UTF-16 key, DWORD alignment, the value
//! region (`wValueLength` bytes), DWORD alignment, then the child nodes.
//!
//! Parsing is best-effort like the sibling resource parsers: a malformed or
//! out-of-bounds child is skipped, and only a fundamentally broken root fails
//! the parse. [`query_version_value`] shares the same walker and reports the
//! *offset* of a matched value so a caller can translate it back into a guest
//! pointer (the faithful `VerQueryValue` semantics — the returned pointer
//! points into the block the caller copied).

use crate::PeSectionMap;

use super::common::{MAX_STRING_WORDS, RT_VERSION, parse_resource_type, read_u16_at, read_u32_at};

/// Byte size of `VS_FIXEDFILEINFO` (13 `DWORD`s; verrsrc.h).
pub const FIXED_FILE_INFO_SIZE: usize = 52;

/// `VS_FIXEDFILEINFO::dwSignature` (verrsrc.h `VS_FFI_SIGNATURE`).
pub const FIXED_FILE_INFO_SIGNATURE: u32 = 0xFEEF_04BD;

/// Safety cap on the `VS_VERSIONINFO` nesting depth. A real tree is at most
/// 4 deep (root → StringFileInfo → language block → string entry); a hostile
/// resource cannot force unbounded recursion.
const MAX_NESTING: usize = 8;

/// One parsed `VS_FIXEDFILEINFO` value.
///
/// Layout (verrsrc.h), 13 little-endian `DWORD`s at 52 bytes total. Offsets
/// verified against the SDK header — the `WIN32_FIND_DATA` precedent: a wrong
/// offset here corrupts `dwFileVersionMS/LS` for every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedFileInfo {
    /// `dwSignature` — must be [`FIXED_FILE_INFO_SIGNATURE`].
    pub signature: u32,
    /// `dwStrucVersion` — binary version of the structure (`0x00010000`).
    pub struc_version: u32,
    /// `dwFileVersionMS` — major.minor of the file version.
    pub file_version_ms: u32,
    /// `dwFileVersionLS` — build.revision of the file version.
    pub file_version_ls: u32,
    /// `dwProductVersionMS` — major.minor of the product version.
    pub product_version_ms: u32,
    /// `dwProductVersionLS` — build.revision of the product version.
    pub product_version_ls: u32,
    /// `dwFileFlagsMask` — valid bits in `file_flags`.
    pub file_flags_mask: u32,
    /// `dwFileFlags` — `VS_FF_*` flags (debug, prerelease, …).
    pub file_flags: u32,
    /// `dwFileOS` — the OS the file was designed for (`VOS_*`).
    pub file_os: u32,
    /// `dwFileType` — `VFT_*` (application, dll, …).
    pub file_type: u32,
    /// `dwFileSubtype` — `VFT2_*` (only meaningful for drivers).
    pub file_subtype: u32,
    /// `dwFileDateMS` — high 32 bits of the file date stamp.
    pub file_date_ms: u32,
    /// `dwFileDateLS` — low 32 bits of the file date stamp.
    pub file_date_ls: u32,
}

impl FixedFileInfo {
    /// Decode a `VS_FIXEDFILEINFO` from the value region starting at `pos`.
    #[must_use]
    pub fn from_bytes(block: &[u8], pos: usize) -> Option<Self> {
        let end = pos.checked_add(FIXED_FILE_INFO_SIZE)?;
        if end > block.len() {
            return None;
        }
        Some(Self {
            signature: read_u32_at(block, pos)?,
            struc_version: read_u32_at(block, pos.checked_add(4)?)?,
            file_version_ms: read_u32_at(block, pos.checked_add(8)?)?,
            file_version_ls: read_u32_at(block, pos.checked_add(12)?)?,
            product_version_ms: read_u32_at(block, pos.checked_add(16)?)?,
            product_version_ls: read_u32_at(block, pos.checked_add(20)?)?,
            file_flags_mask: read_u32_at(block, pos.checked_add(24)?)?,
            file_flags: read_u32_at(block, pos.checked_add(28)?)?,
            file_os: read_u32_at(block, pos.checked_add(32)?)?,
            file_type: read_u32_at(block, pos.checked_add(36)?)?,
            file_subtype: read_u32_at(block, pos.checked_add(40)?)?,
            file_date_ms: read_u32_at(block, pos.checked_add(44)?)?,
            file_date_ls: read_u32_at(block, pos.checked_add(48)?)?,
        })
    }
}

/// One `StringFileInfo` string-table entry (a key/value pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTableEntry {
    /// The string name (e.g. `CompanyName`, `FileVersion`).
    pub key: String,
    /// The decoded value (NUL stripped).
    pub value: String,
}

/// One language block of the `StringFileInfo` section.
///
/// The node key is the `wKey` of the block — the `<lang>-<codepage>` hex
/// string (e.g. `040904b0`) that `\StringFileInfo\<lang>-<codepage>\<key>`
/// paths address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTableBlock {
    /// The block key (e.g. `040904b0`).
    pub lang_codepage: String,
    /// The block's string entries.
    pub entries: Vec<StringTableEntry>,
}

/// Parsed view of a `VS_VERSIONINFO` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionInfo {
    /// The root's `VS_FIXEDFILEINFO` value, when present (52-byte value).
    pub fixed: Option<FixedFileInfo>,
    /// The `StringFileInfo` language blocks, in directory order.
    pub string_blocks: Vec<StringTableBlock>,
    /// `(language, codepage)` pairs from `\VarFileInfo\Translation`.
    pub translation: Vec<(u16, u16)>,
}

/// A located `RT_VERSION` resource: the raw block plus its parsed view.
///
/// The raw bytes are what `GetFileVersionInfo` copies into the guest buffer —
/// the faithful API hands the caller the resource bytes exactly as stored, so
/// the handlers never rebuild the block from the parsed structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionResource {
    /// The raw `VS_VERSIONINFO` block bytes.
    pub raw: Vec<u8>,
    /// Parsed view of [`Self::raw`].
    pub info: VersionInfo,
}

/// One node of a `VS_VERSIONINFO` tree (shared by the parse and the query).
#[derive(Debug)]
struct VersionNode {
    /// `start + wLength`, clamped to the block end.
    end: usize,
    /// Byte length of the value region (0 = no value). For string-table
    /// entries this is `wValueLength * 2` — see [`parse_node`].
    value_len: usize,
    /// Decoded node key (e.g. `VS_VERSION_INFO`, `StringFileInfo`, `040904b0`).
    key: String,
    /// Offset of the value region (absent when `value_len` is 0).
    value: Option<usize>,
    /// Offset of the first child (absent when the node has no children).
    children: Option<usize>,
}

/// Result of a `VerQueryValue`-style path walk: where a matched value sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionQueryMatch {
    /// Offset of the matched value within the block (relative to its start).
    pub offset: usize,
    /// Byte length of the matched value.
    pub len: usize,
}

/// Parse a `VS_VERSIONINFO` block (the root node of the tree).
///
/// Returns `None` when the bytes do not form a version block (missing header,
/// wrong root key, or a structure truncated past recovery) — the caller treats
/// that as "no version information".
#[must_use]
pub fn parse_version_info(block: &[u8]) -> Option<VersionInfo> {
    let root = parse_node(block, 0, 0, false)?;
    if !root.key.eq_ignore_ascii_case("VS_VERSION_INFO") {
        return None;
    }
    let fixed = if root.value_len >= FIXED_FILE_INFO_SIZE {
        root.value.and_then(|v| FixedFileInfo::from_bytes(block, v))
    } else {
        None
    };
    let mut info = VersionInfo {
        fixed,
        ..VersionInfo::default()
    };
    for child in parse_children(block, &root, 0, false) {
        if child.key.eq_ignore_ascii_case("StringFileInfo") {
            for lang in parse_children(block, &child, 1, false) {
                let mut entries = Vec::new();
                // The language block's children are string entries: their
                // `wValueLength` is a WCHAR count, so the value byte length
                // was already doubled by `parse_node`.
                for entry in parse_children(block, &lang, 2, true) {
                    let Some(value) = entry.value else {
                        continue;
                    };
                    let Some(text) = read_utf16_value(block, value, entry.value_len) else {
                        continue;
                    };
                    entries.push(StringTableEntry {
                        key: entry.key,
                        value: text,
                    });
                }
                info.string_blocks.push(StringTableBlock {
                    lang_codepage: lang.key,
                    entries,
                });
            }
        } else if child.key.eq_ignore_ascii_case("VarFileInfo") {
            for var in parse_children(block, &child, 1, false) {
                if !var.key.eq_ignore_ascii_case("Translation") {
                    continue;
                }
                let Some(value) = var.value else {
                    continue;
                };
                info.translation = decode_translation(block, value, var.value_len);
            }
        }
    }
    Some(info)
}

/// Parse every `RT_VERSION` resource of a PE image, in directory order.
///
/// `sections` is the section map (see [`crate::PeMapPlan::sections`]). A PE
/// usually carries exactly one version resource; the handlers take the first.
#[must_use]
pub fn parse_pe_version_resources(image: &[u8], sections: &[PeSectionMap]) -> Vec<VersionResource> {
    parse_resource_type(image, sections, RT_VERSION, |_id, _lang, bytes| {
        let info = parse_version_info(bytes)?;
        Some(VersionResource {
            raw: bytes.to_vec(),
            info,
        })
    })
}

/// Walk a `VS_VERSIONINFO` block against a `VerQueryValue` path.
///
/// Path forms (case-insensitive keys, like Windows):
/// * `\` — the `VS_FIXEDFILEINFO` value (len [`FIXED_FILE_INFO_SIZE`]).
/// * `\StringFileInfo\<lang>-<codepage>\<key>` — one string-table value
///   (len = `wValueLength`, the UTF-16 bytes including the NUL).
/// * `\VarFileInfo\Translation` — the language/codepage pair array.
///
/// Returns the value's offset within the block, so the caller can translate it
/// into a guest pointer (`block_ptr + offset`) — the faithful semantics where
/// the returned pointer points into the copied block.
#[must_use]
pub fn query_version_value(block: &[u8], path: &str) -> Option<VersionQueryMatch> {
    let root = parse_node(block, 0, 0, false)?;
    if !root.key.eq_ignore_ascii_case("VS_VERSION_INFO") {
        return None;
    }
    let components: Vec<&str> = path.split('\\').filter(|c| !c.is_empty()).collect();
    match components.as_slice() {
        [] => {
            let value = root.value?;
            if root.value_len < FIXED_FILE_INFO_SIZE {
                return None;
            }
            let fixed = FixedFileInfo::from_bytes(block, value)?;
            if fixed.signature != FIXED_FILE_INFO_SIGNATURE {
                return None;
            }
            Some(VersionQueryMatch {
                offset: value,
                len: FIXED_FILE_INFO_SIZE,
            })
        }
        [first, lang, key] if first.eq_ignore_ascii_case("StringFileInfo") => {
            let string_info = find_child(block, &root, "StringFileInfo", false)?;
            let lang_block = find_child(block, &string_info, lang, false)?;
            let entry = find_child(block, &lang_block, key, true)?;
            Some(VersionQueryMatch {
                offset: entry.value?,
                len: entry.value_len,
            })
        }
        [first, second]
            if first.eq_ignore_ascii_case("VarFileInfo")
                && second.eq_ignore_ascii_case("Translation") =>
        {
            let var_info = find_child(block, &root, "VarFileInfo", false)?;
            let translation = find_child(block, &var_info, "Translation", false)?;
            Some(VersionQueryMatch {
                offset: translation.value?,
                len: translation.value_len,
            })
        }
        _ => None,
    }
}

/// Parse the node starting at `pos` (bounds- and depth-checked).
///
/// `value_is_text` selects the `wValueLength` interpretation: a string-table
/// entry stores the length of its UTF-16 value in **WCHARs** (verified against
/// both mingw `windres` and MSVC-built `7za.exe` — `"WIE Test\0"` carries
/// `wValueLength = 9` for its 18 value bytes), while every binary value (the
/// fixed info, the `Translation` array) stores bytes. The node's byte length
/// is what bounds the value read and the next-sibling offset.
fn parse_node(block: &[u8], pos: usize, depth: usize, value_is_text: bool) -> Option<VersionNode> {
    if depth > MAX_NESTING {
        return None;
    }
    let total_len = usize::from(read_u16_at(block, pos)?);
    if total_len < 6 {
        return None;
    }
    let value_len = usize::from(read_u16_at(block, pos.checked_add(2)?)?);
    let value_bytes = if value_is_text {
        value_len.checked_mul(2)?
    } else {
        value_len
    };
    // `wType` at +4 (0 binary / 1 text) is not validated: both carry a key
    // and a value, and callers must not distinguish them for the query.
    let end = pos
        .checked_add(total_len)?
        // Clamp so a corrupt `wLength` cannot read past the block.
        .min(block.len());
    if end < pos {
        return None;
    }
    // Key: NUL-terminated UTF-16 starting at +6.
    let mut p = pos.checked_add(6)?;
    let mut units = Vec::new();
    loop {
        if p >= end {
            // No terminator inside the node: not a valid block.
            return None;
        }
        let unit = read_u16_at(block, p)?;
        p = p.checked_add(2)?;
        if unit == 0 {
            break;
        }
        units.push(unit);
        if units.len() > MAX_STRING_WORDS {
            return None;
        }
    }
    let key = String::from_utf16_lossy(&units);
    let aligned = align4(p);
    if aligned > end {
        return None;
    }
    let value = if value_bytes == 0 {
        None
    } else {
        let value_end = aligned.checked_add(value_bytes)?;
        if value_end > end {
            return None;
        }
        Some(aligned)
    };
    let children = match value {
        Some(v) => align4(v.checked_add(value_bytes)?),
        None => aligned,
    };
    Some(VersionNode {
        end,
        value_len: value_bytes,
        key,
        value,
        children: (children < end).then_some(children),
    })
}

/// Parse the children of `node` (stops at the first malformed child).
///
/// `value_is_text` is passed through to [`parse_node`]: the children of a
/// language block are string-table entries (WCHAR-counted values); every other
/// node's children carry binary values (byte-counted).
fn parse_children(
    block: &[u8],
    node: &VersionNode,
    depth: usize,
    value_is_text: bool,
) -> Vec<VersionNode> {
    let mut out = Vec::new();
    let Some(mut pos) = node.children else {
        return out;
    };
    while pos < node.end {
        let Some(child) = parse_node(block, pos, depth + 1, value_is_text) else {
            break;
        };
        // A child claiming a zero or negative length cannot advance; bailing
        // out avoids a spin on a corrupt `wLength`.
        if child.end <= pos {
            break;
        }
        // Sibling alignment: `windres` can leave the value-end padding of a
        // string entry OUT of its `wLength` (verified against the micro and
        // `7za.exe` layouts), so the next sibling sits at the 4-byte boundary
        // after the node, not exactly at `start + wLength`.
        pos = align4(child.end);
        out.push(child);
    }
    out
}

/// Find the first child of `node` whose key matches `key` (case-insensitive).
fn find_child(
    block: &[u8],
    node: &VersionNode,
    key: &str,
    value_is_text: bool,
) -> Option<VersionNode> {
    parse_children(block, node, 0, value_is_text)
        .into_iter()
        .find(|child| child.key.eq_ignore_ascii_case(key))
}

/// Round `v` up to the next 4-byte boundary (the `VS_VERSIONINFO` alignment).
fn align4(v: usize) -> usize {
    v.checked_add(3).map_or(v, |x| x & !3)
}

/// Decode a UTF-16 value (NUL-terminated within `len` bytes) from `pos`.
fn read_utf16_value(block: &[u8], pos: usize, len: usize) -> Option<String> {
    let end = pos.checked_add(len)?;
    if end > block.len() {
        return None;
    }
    let mut units = Vec::new();
    let mut p = pos;
    while p < end {
        let unit = read_u16_at(block, p)?;
        p = p.checked_add(2)?;
        if unit == 0 {
            break;
        }
        units.push(unit);
        if units.len() > MAX_STRING_WORDS {
            return None;
        }
    }
    Some(String::from_utf16_lossy(&units))
}

/// Decode a `Translation` value: consecutive `(language, codepage)` pairs,
/// each packed as one little-endian `DWORD` (low 16 bits = language).
fn decode_translation(block: &[u8], pos: usize, len: usize) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let Some(end) = pos.checked_add(len) else {
        return out;
    };
    let end = end.min(block.len());
    let mut p = pos;
    while p.checked_add(4).is_some_and(|e| e <= end) {
        let Some(word) = read_u32_at(block, p) else {
            break;
        };
        let lang = u16::try_from(word & 0xFFFF).unwrap_or(0);
        let codepage = u16::try_from((word >> 16) & 0xFFFF).unwrap_or(0);
        out.push((lang, codepage));
        let Some(next) = p.checked_add(4) else {
            break;
        };
        p = next;
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::common::test_util::*;
    use super::*;

    /// A UTF-16 NUL-terminated value region (what a string entry stores).
    fn utf16_value(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        put_utf16(&mut v, s);
        v
    }

    /// Pad `buf` to a 4-byte boundary (the `VS_VERSIONINFO` alignment).
    fn pad4(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    /// Patch a `u16` back into `buf` at `pos` (the `wLength` write-back).
    fn patch_u16(buf: &mut [u8], pos: usize, v: u16) {
        let end = pos.checked_add(2).expect("patch range fits");
        let dst = buf.get_mut(pos..end).expect("patch range fits");
        dst.copy_from_slice(&v.to_le_bytes());
    }

    /// Append one `VS_VERSIONINFO` node: header, key, aligned value (optional),
    /// then pre-built children (optional). `wLength` is patched last.
    ///
    /// `value_is_text` mirrors the real-binary quirk: a string-table entry's
    /// `wValueLength` is its value length in WCHARs (half the byte length),
    /// while binary values (fixed info, `Translation`) count bytes.
    fn append_node(
        buf: &mut Vec<u8>,
        key: &str,
        value: Option<&[u8]>,
        value_is_text: bool,
        children: Option<&[u8]>,
    ) {
        let len_pos = buf.len();
        put_u16(buf, 0); // wLength — patched below
        let value_len = value.map_or(0, <[u8]>::len);
        let stored_len = if value_is_text {
            value_len / 2
        } else {
            value_len
        };
        put_u16(
            buf,
            u16::try_from(stored_len).expect("value length fits u16"),
        );
        put_u16(buf, 1); // wType = text
        put_utf16(buf, key);
        pad4(buf);
        if let Some(v) = value {
            buf.extend_from_slice(v);
            pad4(buf);
        }
        if let Some(c) = children {
            buf.extend_from_slice(c);
        }
        let total = buf.len() - len_pos;
        patch_u16(buf, len_pos, u16::try_from(total).expect("total fits u16"));
    }

    /// Build a full sample `VS_VERSIONINFO` tree: file version 1.2.3.4,
    /// product 5.6.7.8, three string entries, one translation pair.
    fn sample_block() -> Vec<u8> {
        let mut fixed = Vec::new();
        put_u32(&mut fixed, FIXED_FILE_INFO_SIGNATURE);
        put_u32(&mut fixed, 0x0001_0000); // struc version 1.0
        put_u32(&mut fixed, 0x0001_0002); // file version MS 1.2
        put_u32(&mut fixed, 0x0003_0004); // file version LS 3.4
        put_u32(&mut fixed, 0x0005_0006); // product version MS 5.6
        put_u32(&mut fixed, 0x0007_0008); // product version LS 7.8
        for _ in 0..7 {
            put_u32(&mut fixed, 0);
        }

        let mut company = Vec::new();
        append_node(
            &mut company,
            "CompanyName",
            Some(&utf16_value("WIE Test")),
            true,
            None,
        );
        let mut description = Vec::new();
        append_node(
            &mut description,
            "FileDescription",
            Some(&utf16_value("version query micro")),
            true,
            None,
        );
        let mut file_version = Vec::new();
        append_node(
            &mut file_version,
            "FileVersion",
            Some(&utf16_value("1.2.3.4")),
            true,
            None,
        );

        let mut lang_children = company;
        lang_children.extend(description);
        lang_children.extend(file_version);
        let mut lang_block = Vec::new();
        append_node(
            &mut lang_block,
            "040904b0",
            None,
            false,
            Some(&lang_children),
        );

        let mut string_info_children = Vec::new();
        append_node(
            &mut string_info_children,
            "040904b0",
            None,
            false,
            Some(&lang_children),
        );
        let mut string_info = Vec::new();
        append_node(
            &mut string_info,
            "StringFileInfo",
            None,
            false,
            Some(&string_info_children),
        );

        let mut translation_value = Vec::new();
        put_u32(&mut translation_value, 0x04B0_0409); // lang 0x0409, cp 0x04B0
        let mut translation = Vec::new();
        append_node(
            &mut translation,
            "Translation",
            Some(&translation_value),
            false,
            None,
        );
        let var_info_children = translation;
        let mut var_info = Vec::new();
        append_node(
            &mut var_info,
            "VarFileInfo",
            None,
            false,
            Some(&var_info_children),
        );

        let mut children = string_info;
        children.extend(var_info);
        let mut root = Vec::new();
        append_node(
            &mut root,
            "VS_VERSION_INFO",
            Some(&fixed),
            false,
            Some(&children),
        );
        root
    }

    #[test]
    fn parses_fixed_info_and_string_table() {
        let block = sample_block();
        let info = parse_version_info(&block).expect("block parses");
        let fixed = info.fixed.expect("fixed info present");
        assert_eq!(fixed.signature, FIXED_FILE_INFO_SIGNATURE);
        assert_eq!(fixed.file_version_ms, 0x0001_0002);
        assert_eq!(fixed.file_version_ls, 0x0003_0004);
        assert_eq!(fixed.product_version_ms, 0x0005_0006);
        assert_eq!(fixed.product_version_ls, 0x0007_0008);

        assert_eq!(info.string_blocks.len(), 1);
        let lang = &info.string_blocks[0];
        assert_eq!(lang.lang_codepage, "040904b0");
        let by_key = |key: &str| -> &str {
            lang.entries
                .iter()
                .find(|e| e.key == key)
                .map(|e| e.value.as_str())
                .expect("entry present")
        };
        assert_eq!(by_key("CompanyName"), "WIE Test");
        assert_eq!(by_key("FileDescription"), "version query micro");
        assert_eq!(by_key("FileVersion"), "1.2.3.4");

        assert_eq!(info.translation, vec![(0x0409, 0x04B0)]);
    }

    /// The fixed-info offsets are ABI: field `i` must sit at byte offset
    /// `i * 4` inside the 52-byte value, per the SDK header.
    #[test]
    fn fixed_info_offsets_are_sdk_layout() {
        let mut value = Vec::new();
        for i in 0..13 {
            put_u32(&mut value, 0x0100_0000 + i);
        }
        let mut root = Vec::new();
        append_node(&mut root, "VS_VERSION_INFO", Some(&value), false, None);
        let info = parse_version_info(&root).expect("block parses");
        let fixed = info.fixed.expect("fixed info present");
        let fields = [
            fixed.signature,
            fixed.struc_version,
            fixed.file_version_ms,
            fixed.file_version_ls,
            fixed.product_version_ms,
            fixed.product_version_ls,
            fixed.file_flags_mask,
            fixed.file_flags,
            fixed.file_os,
            fixed.file_type,
            fixed.file_subtype,
            fixed.file_date_ms,
            fixed.file_date_ls,
        ];
        for (i, field) in fields.iter().enumerate() {
            assert_eq!(
                *field,
                0x0100_0000 + i as u32,
                "field {i} must decode from byte offset {}",
                i * 4
            );
        }
    }

    #[test]
    fn malformed_blocks_yield_none() {
        assert!(parse_version_info(&[]).is_none());
        assert!(parse_version_info(&[0_u8; 8]).is_none());
        // A well-formed node whose key is not VS_VERSION_INFO.
        let mut wrong = Vec::new();
        append_node(&mut wrong, "NotVersionInfo", None, false, None);
        assert!(parse_version_info(&wrong).is_none());
        // A root whose value claims more bytes than the block holds.
        let mut truncated = Vec::new();
        truncated.extend_from_slice(&0_u16.to_le_bytes()); // wLength 0
        assert!(parse_version_info(&truncated).is_none());
        // A node with a corrupt wLength that overruns the block.
        let mut overrun = Vec::new();
        put_u16(&mut overrun, 0xFFFF); // wLength way beyond the data
        put_u16(&mut overrun, 52);
        put_u16(&mut overrun, 0);
        put_utf16(&mut overrun, "VS_VERSION_INFO");
        // Clamped to the block end → the key never terminates → None.
        assert!(parse_version_info(&overrun).is_none());
    }

    #[test]
    fn query_root_returns_fixed_info() {
        let block = sample_block();
        let m = query_version_value(&block, "\\").expect("root query");
        assert_eq!(m.len, FIXED_FILE_INFO_SIZE);
        let fixed = FixedFileInfo::from_bytes(&block, m.offset).expect("decode at offset");
        assert_eq!(fixed.file_version_ms, 0x0001_0002);
    }

    #[test]
    fn query_string_table_paths() {
        let block = sample_block();
        let m = query_version_value(&block, r"\StringFileInfo\040904b0\CompanyName")
            .expect("company name");
        let text = read_utf16_value(&block, m.offset, m.len).expect("decode value");
        assert_eq!(text, "WIE Test");
        // The value length covers the NUL (faithful VerQueryValueW len).
        assert_eq!(m.len, utf16_value("WIE Test").len());

        let m = query_version_value(&block, r"\StringFileInfo\040904b0\FileVersion")
            .expect("file version");
        let text = read_utf16_value(&block, m.offset, m.len).expect("decode value");
        assert_eq!(text, "1.2.3.4");
    }

    #[test]
    fn query_is_case_insensitive_and_tolerates_lang_forms() {
        let block = sample_block();
        // Upper-case key and lang-codepage (the directory uses lowercase hex).
        let m = query_version_value(&block, r"\STRINGFILEINFO\040904B0\COMPANYNAME")
            .expect("case-insensitive match");
        let text = read_utf16_value(&block, m.offset, m.len).expect("decode value");
        assert_eq!(text, "WIE Test");
        // The exact translation-pair form a caller builds from VarFileInfo.
        assert!(query_version_value(&block, r"\StringFileInfo\040904b0\FileVersion").is_some());
        // An unknown language block or key misses.
        assert!(query_version_value(&block, r"\StringFileInfo\00000000\FileVersion").is_none());
        assert!(query_version_value(&block, r"\StringFileInfo\040904b0\NoSuchKey").is_none());
    }

    #[test]
    fn query_var_file_info_translation() {
        let block = sample_block();
        let m = query_version_value(&block, r"\VarFileInfo\Translation").expect("translation");
        assert_eq!(m.len, 4);
        let word = read_u32_at(&block, m.offset).expect("read pair");
        assert_eq!(word, 0x04B0_0409);
    }

    #[test]
    fn query_unknown_paths_yield_none() {
        let block = sample_block();
        assert!(query_version_value(&block, r"\NoSuch").is_none());
        assert!(query_version_value(&block, r"\StringFileInfo").is_none());
        assert!(query_version_value(&block, r"\StringFileInfo\040904b0").is_none());
        assert!(query_version_value(&block, r"\VarFileInfo").is_none());
        assert!(query_version_value(&block, r"\VarFileInfo\Translation\extra").is_none());
    }

    #[test]
    fn query_matches_non_ascii_keys() {
        // A caller decodes the A-path via cp1252 first, so a non-ASCII key
        // reaches the walk as the same Rust char the UTF-16 block stores.
        let mut value = Vec::new();
        append_node(&mut value, "café", Some(&utf16_value("x")), true, None);
        let lang_children = value;
        let mut lang_block = Vec::new();
        append_node(
            &mut lang_block,
            "040904b0",
            None,
            false,
            Some(&lang_children),
        );
        let mut string_info_children = Vec::new();
        append_node(
            &mut string_info_children,
            "040904b0",
            None,
            false,
            Some(&lang_children),
        );
        let mut string_info = Vec::new();
        append_node(
            &mut string_info,
            "StringFileInfo",
            None,
            false,
            Some(&string_info_children),
        );
        let mut root = Vec::new();
        append_node(
            &mut root,
            "VS_VERSION_INFO",
            None,
            false,
            Some(&string_info),
        );

        let m = query_version_value(&root, r"\StringFileInfo\040904b0\café")
            .expect("non-ascii key matches");
        assert_eq!(m.len, utf16_value("x").len());
    }

    /// Parse an `RT_VERSION` resource out of a synthetic image with a resource
    /// tree, exercising the full `parse_pe_version_resources` walk.
    #[test]
    fn parse_pe_version_resources_finds_the_leaf() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1400];

        // Root dir @0x200: type RT_VERSION → type dir @0x218.
        let mut root = Vec::new();
        push_dir_entry(&mut root, u32::from(RT_VERSION), 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: template id 1 → lang dir @0x240.
        let mut type_dir = Vec::new();
        push_dir_entry(&mut type_dir, 1, 0x8000_0040);
        copy_into(&mut image, 0x218, &type_dir);

        // Lang dir @0x240: en-US 0x0409 → data entry @0x268.
        let mut lang_dir = Vec::new();
        push_dir_entry(&mut lang_dir, 0x0409, 0x68);
        copy_into(&mut image, 0x240, &lang_dir);

        // Data entry @0x268 → block body at rva 0x1200 (file 0x400).
        let mut data = Vec::new();
        put_u32(&mut data, 0x1200);
        put_u32(&mut data, 0); // patched to the real size below
        put_u32(&mut data, 0); // code page
        put_u32(&mut data, 0); // reserved
        copy_into(&mut image, 0x268, &data);

        let block = sample_block();
        copy_into(&mut image, 0x400, &block);
        copy_into(
            &mut image,
            0x26C,
            &u32::try_from(block.len()).expect("size fits").to_le_bytes(),
        );

        let resources = parse_pe_version_resources(&image, &sections);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].raw, block);
        assert_eq!(
            resources[0].info.fixed.expect("fixed info").file_version_ms,
            0x0001_0002
        );

        // An image without a version resource yields nothing.
        assert!(parse_pe_version_resources(&[0_u8; 64], &[]).is_empty());
    }

    /// Parse the actual `windres`-built micro (the real-binary format with the
    /// WCHAR-counted string `wValueLength` quirk), when the micro suite has
    /// been built. Skips silently otherwise (like the sibling micro tests).
    #[test]
    fn parses_the_windres_built_micro_version_block() {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop();
        path.pop();
        path.push("micro-exes/out/version_query.exe");
        if !path.is_file() {
            return;
        }
        let bytes = std::fs::read(&path).expect("read micro exe");
        let plan = crate::pe_map_plan_from_bytes(&bytes).expect("parse micro PE");
        let resources = parse_pe_version_resources(&bytes, &plan.sections);
        let resource = resources.first().expect("version resource present");
        let info = &resource.info;
        let fixed = info.fixed.expect("fixed info");
        assert_eq!(fixed.file_version_ms, 0x0001_0002);
        assert_eq!(fixed.file_version_ls, 0x0003_0004);
        let lang = info.string_blocks.first().expect("string block");
        assert_eq!(lang.lang_codepage, "040904b0");
        let by_key = |key: &str| -> String {
            lang.entries
                .iter()
                .find(|e| e.key == key)
                .map(|e| e.value.clone())
                .unwrap_or_default()
        };
        assert_eq!(by_key("CompanyName"), "WIE Test");
        assert_eq!(by_key("FileVersion"), "1.2.3.4");
        assert_eq!(info.translation, vec![(0x0409, 0x04B0)]);
        // The query walker reads the same real block.
        let m = query_version_value(&resource.raw, r"\StringFileInfo\040904b0\FileVersion")
            .expect("query the real block");
        assert_eq!(m.len, 16);
        let text = read_utf16_value(&resource.raw, m.offset, m.len).expect("decode value");
        assert_eq!(text, "1.2.3.4");
    }
}
