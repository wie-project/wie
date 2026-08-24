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

/// Type id of `RT_VERSION` resources in the resource directory
/// (`MAKEINTRESOURCE(16)`; the version resource is the `VS_VERSION_INFO`
/// tree — see `super::version`).
pub(crate) const RT_VERSION: u16 = 16;

/// `IMAGE_SCN_CNT_INITIALIZED_DATA` (used to spot a resource-like section).
const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;

/// High bit of a resource directory entry's `Name` / `OffsetToData` field.
///
/// On `Name` it is `IMAGE_RESOURCE_NAME_IS_STRING` (the name is a string);
/// on `OffsetToData` it is `IMAGE_RESOURCE_DATA_IS_DIRECTORY` (the target is
/// a subdirectory). Both constants are `0x80000000` in the PE spec.
pub(crate) const RESOURCE_ENTRY_HIGH_BIT: u32 = 0x8000_0000;

/// Mask stripping the high bit from an `OffsetToData` field, leaving the
/// offset relative to the resource root directory.
const RESOURCE_ENTRY_OFFSET_MASK: u32 = 0x7FFF_FFFF;

/// `IMAGE_NT_OPTIONAL_HDR64_MAGIC` — PE32+ optional-header magic.
const IMAGE_NT_OPTIONAL_HDR64_MAGIC: u16 = 0x20B;

/// DOS header `e_lfanew` field offset (points at the `PE\0\0` signature).
const E_LFANEW_OFFSET: usize = 0x3C;

/// The `PE\0\0` signature DWORD at `e_lfanew` (`IMAGE_NT_SIGNATURE`).
const PE_SIGNATURE: &[u8] = b"PE\0\0";

/// Byte size of the `PE\0\0` signature (one DWORD).
const PE_SIGNATURE_SIZE: usize = 4;

/// `IMAGE_SIZEOF_FILE_HEADER` — COFF file-header size (optional header follows).
const IMAGE_SIZEOF_FILE_HEADER: usize = 20;

/// PE32+ optional header: offset of `NumberOfRvaAndSizes` (4 bytes before
/// the data-directory table).
const PE32P_OPT_NUM_RVA_AND_SIZES_OFF: usize = 108;

/// PE32+ optional header: offset of the data-directory table.
const PE32P_OPT_DATA_DIRECTORY_OFF: usize = 112;

/// `IMAGE_DIRECTORY_ENTRY_RESOURCE` — index of the resource data directory.
const IMAGE_DIRECTORY_ENTRY_RESOURCE: u32 = 2;

/// Size of one `IMAGE_DATA_DIRECTORY` entry (8 bytes: `VirtualAddress` +
/// `Size`).
const IMAGE_DATA_DIRECTORY_SIZE: usize = 8;

/// `IMAGE_RESOURCE_DIRECTORY::NumberOfNamedEntries` — field offset.
const RESOURCE_DIR_NAMED_ENTRIES_OFF: usize = 12;

/// `IMAGE_RESOURCE_DIRECTORY::NumberOfIdEntries` — field offset.
const RESOURCE_DIR_ID_ENTRIES_OFF: usize = 14;

/// Byte size of the fixed `IMAGE_RESOURCE_DIRECTORY` header (16 bytes); the
/// directory entries follow immediately.
const RESOURCE_DIR_HEADER_SIZE: usize = 16;

/// Stride of one `IMAGE_RESOURCE_DIRECTORY_ENTRY` (8 bytes: `Name` +
/// `OffsetToData`).
const RESOURCE_DIR_ENTRY_SIZE: usize = 8;

/// `IMAGE_RESOURCE_DATA_ENTRY::Size` — field offset (4 bytes past the data
/// RVA).
const RESOURCE_DATA_ENTRY_SIZE_OFF: usize = 4;

/// Marker word for ordinal template fields (dialog menus/classes/titles and
/// item classes): a `0xFFFF` word means the next WORD is an ordinal.
pub(crate) const ORDINAL_MARKER: u16 = 0xFFFF;

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
        if (type_off & RESOURCE_ENTRY_HIGH_BIT) == 0 {
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
    let rel = entry_off & RESOURCE_ENTRY_OFFSET_MASK;
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
    if (entry_off & RESOURCE_ENTRY_HIGH_BIT) != 0 {
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
    let size = read_u32_at(walk.image, off.checked_add(RESOURCE_DATA_ENTRY_SIZE_OFF)?)?;
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
    let pe_off = usize::try_from(read_u32_at(image, E_LFANEW_OFFSET)?).ok()?;
    let sig_off = pe_off.checked_add(PE_SIGNATURE_SIZE)?;
    let sig_end = sig_off.checked_add(PE_SIGNATURE_SIZE)?;
    if image.get(sig_off..sig_end) != Some(PE_SIGNATURE) {
        return None;
    }
    // The optional header starts after the signature and the COFF header.
    let opt_off = pe_off
        .checked_add(PE_SIGNATURE_SIZE)?
        .checked_add(IMAGE_SIZEOF_FILE_HEADER)?;
    // PE32+ only; PE32 (0x10B) is rejected by WIE and has different offsets.
    if read_u16_at(image, opt_off)? != IMAGE_NT_OPTIONAL_HDR64_MAGIC {
        return None;
    }
    // NumberOfRvaAndSizes sits 4 bytes before the directory table; the
    // resource directory is index 2 (16 bytes into the table).
    let num_dirs = read_u32_at(image, opt_off.checked_add(PE32P_OPT_NUM_RVA_AND_SIZES_OFF)?)?;
    if num_dirs <= IMAGE_DIRECTORY_ENTRY_RESOURCE {
        return None;
    }
    let dirs_off = opt_off.checked_add(PE32P_OPT_DATA_DIRECTORY_OFF)?;
    let dir_entry_off = dirs_off.checked_add(
        usize::try_from(IMAGE_DIRECTORY_ENTRY_RESOURCE)
            .ok()?
            .checked_mul(IMAGE_DATA_DIRECTORY_SIZE)?,
    )?;
    let root_rva = read_u32_at(image, dir_entry_off)?;
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
    // Fallback profile of a `.rsrc`-like section: initialized data + read,
    // no write bit.
    const INIT_READ: u32 =
        IMAGE_SCN_CNT_INITIALIZED_DATA | crate::SectionCharacteristics::READ.bits();
    for sec in sections {
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
    let named = read_u16_at(image, off.checked_add(RESOURCE_DIR_NAMED_ENTRIES_OFF)?)?;
    let by_id = read_u16_at(image, off.checked_add(RESOURCE_DIR_ID_ENTRIES_OFF)?)?;
    let total = u32::from(named).checked_add(u32::from(by_id))?;
    // Bound the iteration by what the image can physically hold.
    let count = usize::try_from(total)
        .unwrap_or(usize::MAX)
        .min(image.len() / RESOURCE_DIR_ENTRY_SIZE);
    let base = off.checked_add(RESOURCE_DIR_HEADER_SIZE)?;
    let mut entries = Vec::new();
    for i in 0..count {
        let e = base.checked_add(i.checked_mul(RESOURCE_DIR_ENTRY_SIZE)?)?;
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
    if (name & RESOURCE_ENTRY_HIGH_BIT) != 0 {
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
/// Delegates the per-section extent mapping to [`crate::map_rva_in_section`]
/// (the same `max(virtual_size, raw_size)` logic as the loader's section-table
/// walk) and adds the image-bounds check the resource walker needs, since it
/// reads directly out of the file bytes.
fn rva_to_file(image: &[u8], sections: &[PeSectionMap], rva: u32) -> Option<usize> {
    let raw = sections.iter().find_map(|sec| {
        crate::map_rva_in_section(
            u64::from(rva),
            sec.va,
            sec.virtual_size,
            sec.size_of_raw_data,
            sec.pointer_to_raw_data,
        )
    })?;
    let off = usize::try_from(raw).ok()?;
    (off < image.len()).then_some(off)
}

/// Read `N` raw bytes at `pos` (bounds-checked), like [`crate::read_array`]
/// but returning `None` on any out-of-range read.
fn read_array_at<const N: usize>(bytes: &[u8], pos: usize) -> Option<[u8; N]> {
    crate::read_array::<N>(bytes, pos).ok()
}

pub(crate) fn read_u16_at(bytes: &[u8], pos: usize) -> Option<u16> {
    let raw = read_array_at::<2>(bytes, pos)?;
    Some(u16::from_le_bytes(raw))
}

pub(crate) fn read_u32_at(bytes: &[u8], pos: usize) -> Option<u32> {
    let raw = read_array_at::<4>(bytes, pos)?;
    Some(u32::from_le_bytes(raw))
}

pub(crate) fn read_i16_at(bytes: &[u8], pos: usize) -> Option<i16> {
    let raw = read_array_at::<2>(bytes, pos)?;
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
            // `.rsrc` profile: initialized data + read, no write bit.
            characteristics: super::IMAGE_SCN_CNT_INITIALIZED_DATA
                | crate::SectionCharacteristics::READ.bits(),
            final_protect: crate::PAGE_READWRITE,
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
