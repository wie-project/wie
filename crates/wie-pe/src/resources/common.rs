//! Shared resource-tree walk and low-level byte readers.
//!
//! The `.rsrc` section stores resources as a three-level
//! `IMAGE_RESOURCE_DIRECTORY` tree (type → template id → language) with
//! `IMAGE_RESOURCE_DATA_ENTRY` leaves. This module locates the root directory,
//! descends the tree, and hands each data leaf to a per-type template parser
//! (`super::dialog`, `super::menu`, `super::string`, `super::accel`). The
//! type-id constants and the `read_*` helpers are shared by every parser.

use crate::PeSectionMap;

/// Type id of `RT_DIALOG` resources in the resource directory (`winuser.h`:
/// `MAKEINTRESOURCE(5)`; 16 is `RT_VERSION`).
pub(crate) const RT_DIALOG: u16 = 5;

/// Type id of `RT_MENU` resources in the resource directory
/// (`MAKEINTRESOURCE(4)`). `RT_MENUEX` (11) is the extended variant and is
/// not parsed.
pub(crate) const RT_MENU: u16 = 4;

/// Type id of `RT_STRING` resources in the resource directory
/// (`MAKEINTRESOURCE(6)`).
pub(crate) const RT_STRING: u16 = 6;

/// Type id of `RT_ACCELERATOR` resources in the resource directory
/// (`MAKEINTRESOURCE(9)`).
pub(crate) const RT_ACCELERATOR: u16 = 9;

/// `IMAGE_SCN_CNT_INITIALIZED_DATA` (used to spot a resource-like section).
const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;

/// Safety cap: dialog/menu/string-table strings arrive as length-prefixed
/// bytes from a possibly hostile PE, and a corrupt prefix or a missing NUL
/// terminator must not force unbounded parse-time allocation. 4096 UTF-16
/// words (8 KiB) bounds the scan while sitting far above any realistic field
/// string.
pub(crate) const MAX_STRING_WORDS: usize = 4096;

/// Resource name of a directory entry: an id, or a named (string) resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceName {
    /// Numeric resource id (name level of `RT_DIALOG` templates).
    Id(u16),
    /// Named resource (high bit set in the entry's `Name` field).
    Named,
}

/// Directory contents of an `IMAGE_RESOURCE_DIRECTORY`.
struct ResourceDir {
    /// `(Name, OffsetToData)` pairs, named entries first.
    entries: Vec<(u32, u32)>,
}

/// Walk context for the resource-tree descent.
///
/// Bundles the image bytes, the section map, and the root-directory RVA —
/// every RVA→file-offset translation in the walk needs all three. Carrying
/// them as one value keeps the descent helpers below the `too_many_arguments`
/// lint threshold as the language id threads through.
struct ResourceWalk<'a> {
    image: &'a [u8],
    sections: &'a [PeSectionMap],
    root_rva: u32,
}

/// Parse every resource of `type_id` in `image`, applying `parse_template` to
/// each data leaf.
///
/// `sections` is the section map (see [`crate::PeMapPlan::sections`]).
/// Missing or malformed resource data yields an empty `Vec`; this function
/// never fails the caller.
pub(crate) fn parse_resource_type<T>(
    image: &[u8],
    sections: &[PeSectionMap],
    type_id: u16,
    parse_template: fn(u16, u16, &[u8]) -> Option<T>,
) -> Vec<T> {
    let mut out = Vec::new();
    let Some(root_rva) = resource_root_rva(image, sections) else {
        return out;
    };
    let walk = ResourceWalk {
        image,
        sections,
        root_rva,
    };
    let Some(root_off) = rva_to_file(walk.image, walk.sections, walk.root_rva) else {
        return out;
    };
    let Some(root) = read_resource_dir(walk.image, root_off) else {
        return out;
    };

    for (type_name, type_off) in root.entries {
        if !matches!(
            resource_name(&walk, type_name),
            Some(ResourceName::Id(id)) if id == type_id
        ) {
            continue;
        }
        // The type level must point at a subdirectory (the template-id level).
        if (type_off & 0x8000_0000) == 0 {
            continue;
        }
        let Some(type_dir_off) = entry_target(&walk, type_off) else {
            continue;
        };
        let Some(type_dir) = read_resource_dir(walk.image, type_dir_off) else {
            continue;
        };
        for (id_name, id_off) in type_dir.entries {
            let template_id = match resource_name(&walk, id_name) {
                Some(ResourceName::Id(id)) => id,
                // A named template has no id addressable by id-lookup APIs.
                _ => 0,
            };
            collect_language_leaves(&walk, template_id, id_off, parse_template, &mut out);
        }
    }
    out
}

/// Resolve a directory entry to a file offset.
///
/// Entry offsets are relative to the root resource directory (PE spec), so
/// they are rebased onto the walk's `root_rva` and re-mapped through the
/// section map.
fn entry_target(walk: &ResourceWalk<'_>, entry_off: u32) -> Option<usize> {
    let rel = entry_off & 0x7FFF_FFFF;
    let rva = walk.root_rva.checked_add(rel)?;
    rva_to_file(walk.image, walk.sections, rva)
}

/// Collect templates from a template-id level entry.
///
/// The entry either points directly at an `IMAGE_RESOURCE_DATA_ENTRY` (single
/// language) or at a language subdirectory whose leaves are data entries. The
/// language-level directory entry's **key** is the LANGID (e.g. `0x0007`
/// German, `0x0409` en-US); it becomes the template's `lang` field.
fn collect_language_leaves<T>(
    walk: &ResourceWalk<'_>,
    template_id: u16,
    entry_off: u32,
    parse_template: fn(u16, u16, &[u8]) -> Option<T>,
    out: &mut Vec<T>,
) {
    if (entry_off & 0x8000_0000) != 0 {
        let Some(dir_off) = entry_target(walk, entry_off) else {
            return;
        };
        let Some(dir) = read_resource_dir(walk.image, dir_off) else {
            return;
        };
        for (lang_name, leaf_off) in dir.entries {
            // A named key has no numeric language id; treat it as unknown.
            let lang = match resource_name(walk, lang_name) {
                Some(ResourceName::Id(id)) => id,
                _ => 0,
            };
            push_template_from_leaf(walk, template_id, lang, leaf_off, parse_template, out);
        }
    } else {
        // No language directory: a single data entry with unknown language.
        push_template_from_leaf(walk, template_id, 0, entry_off, parse_template, out);
    }
}

/// Read one `IMAGE_RESOURCE_DATA_ENTRY` leaf and parse its template.
fn push_template_from_leaf<T>(
    walk: &ResourceWalk<'_>,
    template_id: u16,
    lang: u16,
    leaf_off: u32,
    parse_template: fn(u16, u16, &[u8]) -> Option<T>,
    out: &mut Vec<T>,
) {
    let Some((data_rva, data_size)) = read_data_entry(walk, leaf_off) else {
        return;
    };
    let Some(tpl_off) = rva_to_file(walk.image, walk.sections, data_rva) else {
        return;
    };
    let Some(len) = usize::try_from(data_size).ok() else {
        return;
    };
    let Some(end) = tpl_off.checked_add(len) else {
        return;
    };
    let Some(tpl_bytes) = walk.image.get(tpl_off..end) else {
        return;
    };
    if let Some(template) = parse_template(template_id, lang, tpl_bytes) {
        out.push(template);
    }
}

/// Read the `(data RVA, size)` pair of an `IMAGE_RESOURCE_DATA_ENTRY`.
fn read_data_entry(walk: &ResourceWalk<'_>, entry_off: u32) -> Option<(u32, u32)> {
    let off = entry_target(walk, entry_off)?;
    let data_rva = read_u32_at(walk.image, off)?;
    let size = read_u32_at(walk.image, off.checked_add(4)?)?;
    Some((data_rva, size))
}

/// Locate the root resource directory RVA.
///
/// Priority: the optional-header data directory (spec-correct), then a
/// `.rsrc` section by name, then a section with initialized-data + read
/// characteristics and no write bit (the typical `.rsrc` profile).
fn resource_root_rva(image: &[u8], sections: &[PeSectionMap]) -> Option<u32> {
    pe_resource_root_rva(image).or_else(|| resource_section_rva(sections))
}

/// Read `IMAGE_DIRECTORY_ENTRY_RESOURCE` (directory index 2) from the
/// optional header of the PE in `image`.
fn pe_resource_root_rva(image: &[u8]) -> Option<u32> {
    let pe_off = usize::try_from(read_u32_at(image, 0x3C)?).ok()?;
    let sig_off = pe_off.checked_add(4)?;
    let sig_end = sig_off.checked_add(4)?;
    if image.get(sig_off..sig_end) != Some(&b"PE\0\0"[..]) {
        return None;
    }
    // The optional header starts after the 20-byte COFF header.
    let opt_off = pe_off.checked_add(4)?.checked_add(20)?;
    // PE32+ only; PE32 (0x10B) is rejected by WIE and has different offsets.
    if read_u16_at(image, opt_off)? != 0x20B {
        return None;
    }
    // NumberOfRvaAndSizes sits 4 bytes before the directory table; the
    // resource directory is index 2 (16 bytes into the table).
    let num_dirs = read_u32_at(image, opt_off.checked_add(108)?)?;
    if num_dirs <= 2 {
        return None;
    }
    let dirs_off = opt_off.checked_add(112)?;
    let root_rva = read_u32_at(image, dirs_off.checked_add(16)?)?;
    (root_rva != 0).then_some(root_rva)
}

/// Fallback root RVA: the `.rsrc` section, or an initialized+read (no-write)
/// section when no such name exists.
fn resource_section_rva(sections: &[PeSectionMap]) -> Option<u32> {
    for sec in sections {
        if sec.name.trim().eq_ignore_ascii_case(".rsrc") {
            return Some(sec.va);
        }
    }
    for sec in sections {
        const INIT_READ: u32 =
            IMAGE_SCN_CNT_INITIALIZED_DATA | crate::SectionCharacteristics::READ.bits();
        let matches = sec.characteristics & INIT_READ == INIT_READ
            && sec.characteristics & crate::SectionCharacteristics::WRITE.bits() == 0;
        if matches {
            return Some(sec.va);
        }
    }
    None
}

/// Read an `IMAGE_RESOURCE_DIRECTORY` at file offset `off`.
fn read_resource_dir(image: &[u8], off: usize) -> Option<ResourceDir> {
    // Fixed 16-byte header: Characteristics, TimeDateStamp, MajorVersion,
    // MinorVersion, NumberOfNamedEntries, NumberOfIdEntries.
    let named = read_u16_at(image, off.checked_add(12)?)?;
    let by_id = read_u16_at(image, off.checked_add(14)?)?;
    let total = u32::from(named).checked_add(u32::from(by_id))?;
    // Bound the iteration by what the image can physically hold.
    let count = usize::try_from(total)
        .unwrap_or(usize::MAX)
        .min(image.len() >> 3);
    let base = off.checked_add(16)?;
    let mut entries = Vec::new();
    for i in 0..count {
        let e = base.checked_add(i.checked_mul(8)?)?;
        entries.push((
            read_u32_at(image, e)?,
            read_u32_at(image, e.checked_add(4)?)?,
        ));
    }
    Some(ResourceDir { entries })
}

/// Classify a directory entry's `Name` field. Named resources carry a
/// `IMAGE_RESOURCE_DIR_STRING_U` (WORD length + UTF-16 chars) whose offset is
/// relative to the root directory.
fn resource_name(walk: &ResourceWalk<'_>, name: u32) -> Option<ResourceName> {
    if (name & 0x8000_0000) != 0 {
        // Bounds-check the name string without building it (only ids matter).
        let off = entry_target(walk, name)?;
        let len = read_u16_at(walk.image, off)?;
        off.checked_add(2)?
            .checked_add(usize::from(len).checked_mul(2)?)?;
        Some(ResourceName::Named)
    } else {
        Some(ResourceName::Id(u16::try_from(name & 0xFFFF).ok()?))
    }
}

/// Convert an RVA to a file offset via the section map.
///
/// Uses the mapped extent (`max(virtual_size, raw_size)`) and rejects RVAs
/// that fall outside the image bytes.
fn rva_to_file(image: &[u8], sections: &[PeSectionMap], rva: u32) -> Option<usize> {
    for sec in sections {
        let start = sec.va;
        let end = start.checked_add(sec.virtual_size.max(sec.size_of_raw_data))?;
        if rva >= start && rva < end {
            let delta = rva.checked_sub(start)?;
            let raw = u64::from(sec.pointer_to_raw_data).checked_add(u64::from(delta))?;
            let off = usize::try_from(raw).ok()?;
            return (off < image.len()).then_some(off);
        }
    }
    None
}

pub(crate) fn read_u16_at(bytes: &[u8], pos: usize) -> Option<u16> {
    let raw = crate::read_array::<2>(bytes, pos).ok()?;
    Some(u16::from_le_bytes(raw))
}

pub(crate) fn read_u32_at(bytes: &[u8], pos: usize) -> Option<u32> {
    let raw = crate::read_array::<4>(bytes, pos).ok()?;
    Some(u32::from_le_bytes(raw))
}

pub(crate) fn read_i16_at(bytes: &[u8], pos: usize) -> Option<i16> {
    let raw = crate::read_array::<2>(bytes, pos).ok()?;
    Some(i16::from_le_bytes(raw))
}

/// Read a NUL-terminated UTF-16 string at `pos`.
///
/// `seed` supplies the first code unit when the caller already peeked it
/// (field starts are probed for the `0xFFFF` ordinal marker first). A `0`
/// seed is the empty field: a single NUL word, nothing more.
pub(crate) fn read_utf16_string(
    bytes: &[u8],
    pos: usize,
    seed: Option<u16>,
) -> Option<(String, usize)> {
    if seed == Some(0) {
        return Some((String::new(), pos.checked_add(2)?));
    }
    let mut words = Vec::new();
    if let Some(word) = seed {
        words.push(word);
    }
    let mut p = if seed.is_some() {
        pos.checked_add(2)?
    } else {
        pos
    };
    loop {
        let word = read_u16_at(bytes, p)?;
        p = p.checked_add(2)?;
        if word == 0 {
            break;
        }
        words.push(word);
        if words.len() > MAX_STRING_WORDS {
            return None;
        }
    }
    Some((String::from_utf16_lossy(&words), p))
}

/// Shared test builders used by the per-type parser tests in this module
/// tree: synthetic section maps, little-endian field writers, and a synthetic
/// single-entry resource directory.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod test_util {
    use crate::PeSectionMap;

    /// Section map entry covering `[0x1000, 0x2000)` backed by file
    /// `[0x200, 0x1200)` in the fake image.
    pub(crate) fn fake_rsrc_section() -> PeSectionMap {
        PeSectionMap {
            name: ".rsrc".to_owned(),
            va: 0x1000,
            virtual_size: 0x1000,
            pointer_to_raw_data: 0x200,
            size_of_raw_data: 0x1000,
            characteristics: 0x4000_0040,
            final_protect: 0x04,
        }
    }

    pub(crate) fn put_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn put_utf16(buf: &mut Vec<u8>, s: &str) {
        for c in s.encode_utf16() {
            put_u16(buf, c);
        }
        put_u16(buf, 0);
    }

    /// Copy `bytes` into `image` at `start` (bounds-checked).
    pub(crate) fn copy_into(image: &mut [u8], start: usize, bytes: &[u8]) {
        let end = start.checked_add(bytes.len()).expect("range fits");
        image
            .get_mut(start..end)
            .expect("range fits")
            .copy_from_slice(bytes);
    }

    /// Emit an `IMAGE_RESOURCE_DIRECTORY` header with a single id entry.
    pub(crate) fn push_dir_entry(buf: &mut Vec<u8>, id: u32, offset: u32) {
        put_u32(buf, 0); // characteristics
        put_u32(buf, 0); // timestamp
        put_u16(buf, 0); // major version
        put_u16(buf, 0); // minor version
        put_u16(buf, 0); // named entries
        put_u16(buf, 1); // id entries
        put_u32(buf, id);
        put_u32(buf, offset);
    }
}
