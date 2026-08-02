//! RT_DIALOG resource parsing for PE images.
//!
//! Walks the `IMAGE_RESOURCE_DIRECTORY` tree in the `.rsrc` section (PE
//! resource format, Microsoft Learn) and parses each `RT_DIALOG` template
//! (type **5** — note: 16 is `RT_VERSION`) into a [`DialogTemplate`].
//! Parsing is best-effort: malformed or out-of-bounds structures skip that
//! entry, and a missing/malformed resource tree yields an empty `Vec` — a
//! broken resource section never fails the module load.
//!
//! # Template layout notes
//!
//! Two on-disk variants of the standard dialog template exist:
//!
//! * The `winuser.h` `DLGTEMPLATE` layout, emitted by `windres` and MSVC
//!   `rc.exe`: `style, exStyle, cDlgItems, x, y, cx, cy` (18 bytes).
//! * The layout documented on Microsoft Learn, which prefixes
//!   `dlgVer = 1, signature = 0, helpID` (28 bytes).
//!
//! Both are detected and parsed. `DLGTEMPLATEEX` (`dlgVer = 1`,
//! `signature = 0xFFFF`) is not parsed — it is skipped with a comment.
//!
//! Dialog units (DLUs) convert to pixels at the standard `GetDialogBaseUnits`
//! 8×16 font: `px = dlu * base / 4` on the x axis and `dlu * base / 8` on the
//! y axis, which is `dlu * 2` on both axes. Both the raw DLUs and the
//! computed pixel rect are stored.

use crate::PeSectionMap;

/// Type id of `RT_DIALOG` resources in the resource directory (`winuser.h`:
/// `MAKEINTRESOURCE(5)`; 16 is `RT_VERSION`).
const RT_DIALOG: u16 = 5;

/// Window style bits (`WS_*`/`DS_*`, winuser.h) used while parsing dialog
/// templates. Converted to the raw `u32` on [`DialogTemplate`] because the
/// user32 consumer combines them with its own `WS_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowStyle(u32);

impl WindowStyle {
    /// `DS_SETFONT`: the template carries a font point size and face.
    pub(crate) const DS_SETFONT: Self = Self(0x40);

    /// Raw style bits.
    #[must_use]
    pub(crate) const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all bits of `flag` are set.
    #[must_use]
    pub(crate) const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

/// Extended window style bits (`WS_EX_*`, winuser.h) used while parsing dialog
/// templates. Passed through uninterpreted to the raw `u32` field today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowExStyle(u32);

impl WindowExStyle {
    /// Raw extended style bits.
    #[must_use]
    pub(crate) const fn bits(self) -> u32 {
        self.0
    }
}

/// `IMAGE_SCN_CNT_INITIALIZED_DATA` (used to spot a resource-like section).
const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;

/// Safety cap for UTF-16 field strings (matches the 4096-char guest string cap).
const MAX_STRING_WORDS: usize = 4096;

/// Axis pixel size derived from dialog units.
///
/// `x`, `y` are the dialog origin; `cx`, `cy` the size. Stored in pixels
/// because dialog construction works in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    /// Left edge, pixels
    pub x: i32,
    /// Top edge, pixels
    pub y: i32,
    /// Width, pixels
    pub cx: i32,
    /// Height, pixels
    pub cy: i32,
}

/// One parsed `RT_DIALOG` template (standard `DLGTEMPLATE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogTemplate {
    /// Template id (the resource name at the second directory level); named
    /// resources have no numeric id and come back as `0` (unaddressable by
    /// `DialogBoxParam`, which resolves ids only)
    pub name: u16,
    /// Template style (`DS_*`/`WS_*` bits)
    pub style: u32,
    /// Template extended style
    pub ex_style: u32,
    /// Origin x, dialog units
    pub x: i16,
    /// Origin y, dialog units
    pub y: i16,
    /// Width, dialog units
    pub cx: i16,
    /// Height, dialog units
    pub cy: i16,
    /// Caption text (ordinal captions referencing the string table are empty)
    pub title: String,
    /// `DS_SETFONT` point size, when present
    pub font_point: Option<u16>,
    /// `DS_SETFONT` typeface name, when present
    pub font_face: Option<String>,
    /// Origin/size converted to pixels at 2 px per DLU
    pub pixel_rect: PixelRect,
    /// Controls in template order
    pub items: Vec<DialogItemTemplate>,
}

/// One parsed `DLGITEMTEMPLATE` (dialog control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogItemTemplate {
    /// Control id (the `WM_COMMAND`/`GetDlgItem` handle)
    pub id: u16,
    /// Control style bits
    pub style: u32,
    /// Control extended style
    pub ex_style: u32,
    /// Origin x relative to the dialog, dialog units
    pub x: i16,
    /// Origin y relative to the dialog, dialog units
    pub y: i16,
    /// Width, dialog units
    pub cx: i16,
    /// Height, dialog units
    pub cy: i16,
    /// Control class (ordinal or name)
    pub class: ItemClass,
    /// Control text (ordinal titles referencing the string table are empty)
    pub title: String,
    /// Origin/size converted to pixels at 2 px per DLU
    pub pixel_rect: PixelRect,
}

/// Standard control classes. `Other` carries the raw class ordinal; a
/// non-standard class *name* has no ordinal and maps to `Other(0)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemClass {
    /// `BUTTON` (ordinal 0x80).
    Button,
    /// `EDIT` (ordinal 0x81).
    Edit,
    /// `STATIC` (ordinal 0x82).
    Static,
    /// `LISTBOX` (ordinal 0x83).
    ListBox,
    /// `COMBOBOX` (ordinal 0x85).
    ComboBox,
    /// Anything else: the raw class ordinal.
    Other(u16),
}

impl PixelRect {
    /// Convert a DLU rect to pixels at the standard 8×16 base font.
    #[must_use]
    pub fn from_dlu(x: i16, y: i16, cx: i16, cy: i16) -> Self {
        Self {
            x: dlu_to_px(x, 8),
            y: dlu_to_px(y, 16),
            cx: dlu_to_px(cx, 8),
            cy: dlu_to_px(cy, 16),
        }
    }
}

/// Convert dialog units to pixels for one axis.
///
/// Windows dialog units are 1/4 of the base font's average character width
/// (x axis, `base` = 8) or 1/8 of its height (y axis, `base` = 16). With the
/// standard 8×16 font both expressions reduce to `dlu * 2`.
#[must_use]
pub fn dlu_to_px(dlu: i16, base: u32) -> i32 {
    debug_assert!(base == 8 || base == 16, "base font must be 8×16");
    i32::from(dlu).saturating_mul(2)
}

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

/// Parse every `RT_DIALOG` template in `image`.
///
/// `sections` is the section map (see [`crate::PeMapPlan::sections`]).
/// Missing or malformed resource data yields an empty `Vec`; this function
/// never fails the caller.
pub fn parse_dialogs(image: &[u8], sections: &[PeSectionMap]) -> Vec<DialogTemplate> {
    let mut dialogs = Vec::new();
    let Some(root_rva) = resource_root_rva(image, sections) else {
        return dialogs;
    };
    let Some(root_off) = rva_to_file(image, sections, root_rva) else {
        return dialogs;
    };
    let Some(root) = read_resource_dir(image, root_off) else {
        return dialogs;
    };

    for (type_name, type_off) in root.entries {
        if !matches!(
            resource_name(image, sections, root_rva, type_name),
            Some(ResourceName::Id(RT_DIALOG))
        ) {
            continue;
        }
        // The type level must point at a subdirectory (the template-id level).
        if (type_off & 0x8000_0000) == 0 {
            continue;
        }
        let Some(type_dir_off) = entry_target(image, sections, root_rva, type_off) else {
            continue;
        };
        let Some(type_dir) = read_resource_dir(image, type_dir_off) else {
            continue;
        };
        for (id_name, id_off) in type_dir.entries {
            let template_id = match resource_name(image, sections, root_rva, id_name) {
                Some(ResourceName::Id(id)) => id,
                // A named template has no id addressable by DialogBoxParam.
                _ => 0,
            };
            collect_language_leaves(image, sections, root_rva, template_id, id_off, &mut dialogs);
        }
    }
    dialogs
}

/// Resolve a directory entry to a file offset.
///
/// Entry offsets are relative to the root resource directory (PE spec), so
/// they are rebased onto `root_rva` and re-mapped through the section map.
fn entry_target(
    image: &[u8],
    sections: &[PeSectionMap],
    root_rva: u32,
    entry_off: u32,
) -> Option<usize> {
    let rel = entry_off & 0x7FFF_FFFF;
    let rva = root_rva.checked_add(rel)?;
    rva_to_file(image, sections, rva)
}

/// Collect dialog templates from a template-id level entry.
///
/// The entry either points directly at an `IMAGE_RESOURCE_DATA_ENTRY` (single
/// language) or at a language subdirectory whose leaves are data entries.
fn collect_language_leaves(
    image: &[u8],
    sections: &[PeSectionMap],
    root_rva: u32,
    template_id: u16,
    entry_off: u32,
    out: &mut Vec<DialogTemplate>,
) {
    if (entry_off & 0x8000_0000) != 0 {
        let Some(dir_off) = entry_target(image, sections, root_rva, entry_off) else {
            return;
        };
        let Some(dir) = read_resource_dir(image, dir_off) else {
            return;
        };
        for (_, leaf_off) in dir.entries {
            push_dialog_from_leaf(image, sections, root_rva, template_id, leaf_off, out);
        }
    } else {
        push_dialog_from_leaf(image, sections, root_rva, template_id, entry_off, out);
    }
}

/// Read one `IMAGE_RESOURCE_DATA_ENTRY` leaf and parse its template.
fn push_dialog_from_leaf(
    image: &[u8],
    sections: &[PeSectionMap],
    root_rva: u32,
    template_id: u16,
    leaf_off: u32,
    out: &mut Vec<DialogTemplate>,
) {
    let Some((data_rva, data_size)) = read_data_entry(image, sections, root_rva, leaf_off) else {
        return;
    };
    let Some(tpl_off) = rva_to_file(image, sections, data_rva) else {
        return;
    };
    let Some(len) = usize::try_from(data_size).ok() else {
        return;
    };
    let Some(end) = tpl_off.checked_add(len) else {
        return;
    };
    let Some(tpl_bytes) = image.get(tpl_off..end) else {
        return;
    };
    if let Some(dialog) = parse_dialog_template(template_id, tpl_bytes) {
        out.push(dialog);
    }
}

/// Read the `(data RVA, size)` pair of an `IMAGE_RESOURCE_DATA_ENTRY`.
fn read_data_entry(
    image: &[u8],
    sections: &[PeSectionMap],
    root_rva: u32,
    entry_off: u32,
) -> Option<(u32, u32)> {
    let off = entry_target(image, sections, root_rva, entry_off)?;
    let data_rva = read_u32_at(image, off)?;
    let size = read_u32_at(image, off.checked_add(4)?)?;
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
fn resource_name(
    image: &[u8],
    sections: &[PeSectionMap],
    root_rva: u32,
    name: u32,
) -> Option<ResourceName> {
    if (name & 0x8000_0000) != 0 {
        // Bounds-check the name string without building it (only ids matter).
        let off = entry_target(image, sections, root_rva, name)?;
        let len = read_u16_at(image, off)?;
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

/// Parse one dialog template from its resource bytes.
///
/// Skips `DLGTEMPLATEEX` and any template whose items run past the
/// byte slice (malformed → treated as absent).
fn parse_dialog_template(template_id: u16, bytes: &[u8]) -> Option<DialogTemplate> {
    let word0 = read_u16_at(bytes, 0)?;
    let word1 = read_u16_at(bytes, 2)?;

    // DLGTEMPLATEEX (dlgVer=1, signature=0xFFFF): not parsed.
    if word0 == 1 && word1 == 0xFFFF {
        return None;
    }

    // Header selection. windres/rc emit the winuser.h DLGTEMPLATE (style
    // first, 18 bytes); the MSDN-documented variant prefixes
    // dlgVer/signature/helpID (28 bytes).
    let (style, ex_style, pos) = if word0 == 1 && word1 == 0 {
        let ex_style = WindowExStyle(read_u32_at(bytes, 8)?);
        let style = WindowStyle(read_u32_at(bytes, 12)?);
        (style, ex_style, 16)
    } else {
        let style = WindowStyle(read_u32_at(bytes, 0)?);
        let ex_style = WindowExStyle(read_u32_at(bytes, 4)?);
        (style, ex_style, 8)
    };

    let item_count = u32::from(read_u16_at(bytes, pos)?);
    let x = read_i16_at(bytes, pos.checked_add(2)?)?;
    let y = read_i16_at(bytes, pos.checked_add(4)?)?;
    let cx = read_i16_at(bytes, pos.checked_add(6)?)?;
    let cy = read_i16_at(bytes, pos.checked_add(8)?)?;
    let mut p = pos.checked_add(10)?;

    // Menu, class, title: each an ordinal (0xFFFF + id) or a UTF-16 string.
    let (_menu, next) = read_optional_text(bytes, p)?;
    p = next;
    let (_class, next) = read_optional_text(bytes, p)?;
    p = next;
    let (title, next) = read_optional_text(bytes, p)?;
    p = next;

    // DS_SETFONT: WORD point size + UTF-16 face (no charset word in the
    // standard template — that exists only in DLGTEMPLATEEX).
    let mut font_point = None;
    let mut font_face = None;
    if style.contains(WindowStyle::DS_SETFONT) {
        font_point = Some(read_u16_at(bytes, p)?);
        let (face, next) = read_utf16_string(bytes, p.checked_add(2)?, None)?;
        font_face = Some(face);
        p = next;
    }

    // Items are DWORD-aligned relative to the start of the template.
    let mut item_pos = p;
    let mut items = Vec::new();
    for _ in 0..item_count {
        item_pos = align4(item_pos)?;
        let (item, next) = parse_dialog_item(bytes, item_pos)?;
        items.push(item);
        item_pos = next;
    }

    Some(DialogTemplate {
        name: template_id,
        style: style.bits(),
        ex_style: ex_style.bits(),
        x,
        y,
        cx,
        cy,
        title,
        font_point,
        font_face,
        pixel_rect: PixelRect::from_dlu(x, y, cx, cy),
        items,
    })
}

/// Parse one `DLGITEMTEMPLATE`, returning the item and the offset just past
/// its creation data.
fn parse_dialog_item(bytes: &[u8], pos: usize) -> Option<(DialogItemTemplate, usize)> {
    let style = WindowStyle(read_u32_at(bytes, pos)?);
    let ex_style = WindowExStyle(read_u32_at(bytes, pos.checked_add(4)?)?);
    let x = read_i16_at(bytes, pos.checked_add(8)?)?;
    let y = read_i16_at(bytes, pos.checked_add(10)?)?;
    let cx = read_i16_at(bytes, pos.checked_add(12)?)?;
    let cy = read_i16_at(bytes, pos.checked_add(14)?)?;
    let id = read_u16_at(bytes, pos.checked_add(16)?)?;
    let mut p = pos.checked_add(18)?;

    let (class, next) = parse_item_class(bytes, p)?;
    p = next;
    let (title, next) = read_optional_text(bytes, p)?;
    p = next;
    let creation_data_size = read_u16_at(bytes, p)?;
    p = p
        .checked_add(2)?
        .checked_add(usize::from(creation_data_size))?;

    Some((
        DialogItemTemplate {
            id,
            style: style.bits(),
            ex_style: ex_style.bits(),
            x,
            y,
            cx,
            cy,
            class,
            title,
            pixel_rect: PixelRect::from_dlu(x, y, cx, cy),
        },
        p,
    ))
}

/// Parse the class field of a dialog item.
///
/// Encodings seen in the wild: `0xFFFF` + ordinal WORD (windres), a single
/// WORD with high byte `0xFF` and the ordinal in the low byte (PE spec), or a
/// NUL-terminated UTF-16 class name.
fn parse_item_class(bytes: &[u8], pos: usize) -> Option<(ItemClass, usize)> {
    let word = read_u16_at(bytes, pos)?;
    if word == 0xFFFF {
        let ordinal = read_u16_at(bytes, pos.checked_add(2)?)?;
        return Some((ItemClass::from_ordinal(ordinal), pos.checked_add(4)?));
    }
    if word >> 8 == 0xFF {
        return Some((ItemClass::from_ordinal(word & 0xFF), pos.checked_add(2)?));
    }
    let (name, next) = read_utf16_string(bytes, pos, Some(word))?;
    let class = match name.to_ascii_lowercase().as_str() {
        "button" => ItemClass::Button,
        "edit" => ItemClass::Edit,
        "static" => ItemClass::Static,
        "listbox" => ItemClass::ListBox,
        "combobox" => ItemClass::ComboBox,
        // String classes beyond the standard five carry no ordinal.
        _ => ItemClass::Other(0),
    };
    Some((class, next))
}

impl ItemClass {
    /// Map a class ordinal to the matching standard variant.
    fn from_ordinal(ordinal: u16) -> Self {
        match ordinal {
            0x0080 => Self::Button,
            0x0081 => Self::Edit,
            0x0082 => Self::Static,
            0x0083 => Self::ListBox,
            0x0085 => Self::ComboBox,
            other => Self::Other(other),
        }
    }
}

/// Read a template field that is either an ordinal (`0xFFFF` + id) or a
/// NUL-terminated UTF-16 string.
///
/// Ordinals reference string-table resources that this parser does not
/// resolve; they come back as empty strings.
fn read_optional_text(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
    let word = read_u16_at(bytes, pos)?;
    if word == 0xFFFF {
        let _ordinal = read_u16_at(bytes, pos.checked_add(2)?)?;
        return Some((String::new(), pos.checked_add(4)?));
    }
    read_utf16_string(bytes, pos, Some(word))
}

/// Read a NUL-terminated UTF-16 string at `pos`.
///
/// `seed` supplies the first code unit when the caller already peeked it
/// (field starts are probed for the `0xFFFF` ordinal marker first). A `0`
/// seed is the empty field: a single NUL word, nothing more.
fn read_utf16_string(bytes: &[u8], pos: usize, seed: Option<u16>) -> Option<(String, usize)> {
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

/// Round `pos` up to the next 4-byte boundary.
fn align4(pos: usize) -> Option<usize> {
    pos.checked_add(3).map(|p| p & !3)
}

fn read_u16_at(bytes: &[u8], pos: usize) -> Option<u16> {
    let raw = crate::read_array::<2>(bytes, pos).ok()?;
    Some(u16::from_le_bytes(raw))
}

fn read_u32_at(bytes: &[u8], pos: usize) -> Option<u32> {
    let raw = crate::read_array::<4>(bytes, pos).ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_i16_at(bytes: &[u8], pos: usize) -> Option<i16> {
    let raw = crate::read_array::<2>(bytes, pos).ok()?;
    Some(i16::from_le_bytes(raw))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Section map entry covering `[0x1000, 0x2000)` backed by file
    /// `[0x200, 0x1200)` in the fake image.
    fn fake_rsrc_section() -> PeSectionMap {
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

    fn put_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_utf16(buf: &mut Vec<u8>, s: &str) {
        for c in s.encode_utf16() {
            put_u16(buf, c);
        }
        put_u16(buf, 0);
    }

    #[test]
    fn dlu_conversion_is_two_pixels_per_unit() {
        assert_eq!(dlu_to_px(0, 8), 0);
        assert_eq!(dlu_to_px(10, 8), 20);
        assert_eq!(dlu_to_px(60, 16), 120);
        assert_eq!(dlu_to_px(-1, 8), -2);
        assert_eq!(
            PixelRect::from_dlu(10, 20, 160, 60),
            PixelRect {
                x: 20,
                y: 40,
                cx: 320,
                cy: 120,
            }
        );
    }

    #[test]
    fn item_class_parses_ordinal_marker() {
        // windres encoding: 0xFFFF marker + ordinal WORD.
        let mut buf = Vec::new();
        put_u16(&mut buf, 0xFFFF);
        put_u16(&mut buf, 0x0080); // BUTTON
        let (class, next) = parse_item_class(&buf, 0).expect("ordinal class");
        assert_eq!(class, ItemClass::Button);
        assert_eq!(next, 4);

        put_u16(&mut buf, 0xFFFF);
        put_u16(&mut buf, 0x0081); // EDIT
        let (class, next) = parse_item_class(&buf, next).expect("ordinal class");
        assert_eq!(class, ItemClass::Edit);

        put_u16(&mut buf, 0xFFFF);
        put_u16(&mut buf, 0x0085); // COMBOBOX
        let (class, next) = parse_item_class(&buf, next).expect("ordinal class");
        assert_eq!(class, ItemClass::ComboBox);

        put_u16(&mut buf, 0xFFFF);
        put_u16(&mut buf, 0x1234); // unknown ordinal
        let (class, next) = parse_item_class(&buf, next).expect("ordinal class");
        assert_eq!(class, ItemClass::Other(0x1234));
        assert_eq!(next, buf.len());
    }

    #[test]
    fn item_class_parses_high_byte_encoding() {
        // PE-spec encoding: high byte 0xFF, ordinal in the low byte.
        let mut buf = Vec::new();
        put_u16(&mut buf, 0xFF82); // STATIC
        let (class, next) = parse_item_class(&buf, 0).expect("high-byte class");
        assert_eq!(class, ItemClass::Static);
        assert_eq!(next, 2);
    }

    #[test]
    fn item_class_parses_string_names() {
        let mut buf = Vec::new();
        put_utf16(&mut buf, "Button");
        let (class, next) = parse_item_class(&buf, 0).expect("named class");
        assert_eq!(class, ItemClass::Button);
        assert_eq!(next, buf.len());

        put_utf16(&mut buf, "msctls_progress32");
        let (class, _) = parse_item_class(&buf, next).expect("custom class");
        assert_eq!(class, ItemClass::Other(0));

        // Empty class string → Other(0).
        let empty = vec![0_u8; 2];
        let (class, _) = parse_item_class(&empty, 0).expect("empty class");
        assert_eq!(class, ItemClass::Other(0));
    }

    #[test]
    fn optional_text_parses_ordinal_and_string() {
        let mut buf = Vec::new();
        put_u16(&mut buf, 0xFFFF);
        put_u16(&mut buf, 7);
        let (text, next) = read_optional_text(&buf, 0).expect("ordinal");
        assert_eq!(text, "");
        assert_eq!(next, 4);

        put_utf16(&mut buf, "Caption");
        let (text, next) = read_optional_text(&buf, next).expect("string");
        assert_eq!(text, "Caption");

        // Empty field (a single NUL word).
        put_u16(&mut buf, 0);
        let (text, next) = read_optional_text(&buf, next).expect("empty");
        assert_eq!(text, "");
        assert_eq!(next, buf.len());
    }

    #[test]
    fn parses_windres_style_template() {
        let mut b = Vec::new();
        put_u32(&mut b, 0x80C0_0040); // style: WS_POPUP|WS_CAPTION|DS_SETFONT
        put_u32(&mut b, 0); // exStyle
        put_u16(&mut b, 1); // one item
        put_u16(&mut b, 10);
        put_u16(&mut b, 20);
        put_u16(&mut b, 100);
        put_u16(&mut b, 40);
        put_u16(&mut b, 0); // menu: absent
        put_u16(&mut b, 0); // class: absent
        put_utf16(&mut b, "Hi");
        put_u16(&mut b, 8); // point size
        put_utf16(&mut b, "Arial");
        while b.len() & 3 != 0 {
            b.push(0);
        }
        // Item: BUTTON "OK" at (5, 5, 50, 14), id 7.
        put_u32(&mut b, 0x5000_0000); // WS_CHILD|WS_VISIBLE
        put_u32(&mut b, 0);
        put_u16(&mut b, 5);
        put_u16(&mut b, 5);
        put_u16(&mut b, 50);
        put_u16(&mut b, 14);
        put_u16(&mut b, 7);
        put_u16(&mut b, 0xFFFF);
        put_u16(&mut b, 0x0080); // BUTTON
        put_utf16(&mut b, "OK");
        put_u16(&mut b, 0); // creation data size

        let t = parse_dialog_template(42, &b).expect("template");
        assert_eq!(t.name, 42);
        assert_eq!(t.style, 0x80C0_0040);
        assert_eq!((t.x, t.y, t.cx, t.cy), (10, 20, 100, 40));
        assert_eq!(t.pixel_rect.cx, 200);
        assert_eq!(t.title, "Hi");
        assert_eq!(t.font_point, Some(8));
        assert_eq!(t.font_face.as_deref(), Some("Arial"));
        assert_eq!(t.items.len(), 1);
        let item = &t.items[0];
        assert_eq!(item.id, 7);
        assert_eq!(item.class, ItemClass::Button);
        assert_eq!(item.title, "OK");
        assert_eq!((item.x, item.y, item.cx, item.cy), (5, 5, 50, 14));
        assert_eq!(item.pixel_rect.cx, 100);
    }

    #[test]
    fn parses_msdn_variant_header() {
        // dlgVer=1, signature=0, helpID, exStyle, style, then the same tail.
        let mut b = Vec::new();
        put_u16(&mut b, 1);
        put_u16(&mut b, 0);
        put_u32(&mut b, 0); // helpID
        put_u32(&mut b, 0); // exStyle
        put_u32(&mut b, 0x40); // DS_SETFONT
        put_u16(&mut b, 0); // no items
        put_u16(&mut b, 1);
        put_u16(&mut b, 2);
        put_u16(&mut b, 3);
        put_u16(&mut b, 4);
        put_u16(&mut b, 0); // menu
        put_u16(&mut b, 0); // class
        put_utf16(&mut b, "T");
        put_u16(&mut b, 8); // point
        put_utf16(&mut b, "A");

        let t = parse_dialog_template(7, &b).expect("msdn variant");
        assert_eq!(t.name, 7);
        assert_eq!(t.title, "T");
        assert_eq!(t.font_point, Some(8));
        assert_eq!(t.font_face.as_deref(), Some("A"));
        assert!(t.items.is_empty());
    }

    #[test]
    fn dlg_template_ex_is_deferred() {
        // dlgVer=1, signature=0xFFFF → skipped (unsupported).
        let bytes = [1_u8, 0, 0xFF, 0xFF];
        assert!(parse_dialog_template(1, &bytes).is_none());
    }

    #[test]
    fn truncated_template_is_not_fatal() {
        // Header claims 3 items but the bytes end after the header.
        let mut b = Vec::new();
        put_u32(&mut b, 0x80C0_0000);
        put_u32(&mut b, 0);
        put_u16(&mut b, 3);
        put_u16(&mut b, 0);
        put_u16(&mut b, 0);
        put_u16(&mut b, 0);
        put_u16(&mut b, 0);
        assert!(parse_dialog_template(1, &b).is_none());
    }

    /// Copy `bytes` into `image` at `start` (bounds-checked).
    fn copy_into(image: &mut [u8], start: usize, bytes: &[u8]) {
        let end = start.checked_add(bytes.len()).expect("range fits");
        image
            .get_mut(start..end)
            .expect("range fits")
            .copy_from_slice(bytes);
    }

    /// Walk a complete synthetic resource tree end-to-end (no real PE).
    #[test]
    fn parses_dialog_from_synthetic_resource_tree() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1200];

        // Root dir @0x200 (rva 0x1000 → file 0x200): type 5 → subdir 0x210.
        let mut root = Vec::new();
        put_u32(&mut root, 0); // characteristics
        put_u32(&mut root, 0); // timestamp
        put_u16(&mut root, 0); // major version
        put_u16(&mut root, 0); // minor version
        put_u16(&mut root, 0); // named entries
        put_u16(&mut root, 1); // id entries
        put_u32(&mut root, u32::from(RT_DIALOG));
        put_u32(&mut root, 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: template id 100 → subdir 0x230.
        let mut type_dir = Vec::new();
        put_u32(&mut type_dir, 0);
        put_u32(&mut type_dir, 0);
        put_u16(&mut type_dir, 0); // major version
        put_u16(&mut type_dir, 0); // minor version
        put_u16(&mut type_dir, 0); // named entries
        put_u16(&mut type_dir, 1); // id entries
        put_u32(&mut type_dir, 100);
        put_u32(&mut type_dir, 0x8000_0030);
        copy_into(&mut image, 0x218, &type_dir);

        // Language dir @0x230: language 0x409 → data entry 0x248.
        let mut lang = Vec::new();
        put_u32(&mut lang, 0);
        put_u32(&mut lang, 0);
        put_u16(&mut lang, 0); // major version
        put_u16(&mut lang, 0); // minor version
        put_u16(&mut lang, 0); // named entries
        put_u16(&mut lang, 1); // id entries
        put_u32(&mut lang, 0x0409);
        put_u32(&mut lang, 0x48);
        copy_into(&mut image, 0x230, &lang);

        // Data entry @0x248: template at rva 0x1200 → file 0x400.
        let mut data = Vec::new();
        put_u32(&mut data, 0x1200);
        put_u32(&mut data, 24); // patched to the real size below
        put_u32(&mut data, 0); // code page
        put_u32(&mut data, 0); // reserved
        copy_into(&mut image, 0x248, &data);

        // Template @0x400 (file) = rva 0x1200: one STATIC item.
        let mut tpl = Vec::new();
        put_u32(&mut tpl, 0x80C0_0000); // style (no DS_SETFONT here)
        put_u32(&mut tpl, 0);
        put_u16(&mut tpl, 1);
        put_u16(&mut tpl, 0);
        put_u16(&mut tpl, 0);
        put_u16(&mut tpl, 160);
        put_u16(&mut tpl, 60);
        put_u16(&mut tpl, 0); // menu
        put_u16(&mut tpl, 0); // class
        put_utf16(&mut tpl, "Dlg");
        while tpl.len() & 3 != 0 {
            tpl.push(0);
        }
        put_u32(&mut tpl, 0x5000_0000);
        put_u32(&mut tpl, 0);
        put_u16(&mut tpl, 10);
        put_u16(&mut tpl, 10);
        put_u16(&mut tpl, 80);
        put_u16(&mut tpl, 20);
        put_u16(&mut tpl, 3);
        put_u16(&mut tpl, 0xFFFF);
        put_u16(&mut tpl, 0x0082); // STATIC
        put_utf16(&mut tpl, "Label");
        put_u16(&mut tpl, 0);
        copy_into(&mut image, 0x400, &tpl);
        // Patch the data entry size to the real template length.
        let size_bytes = u32::try_from(tpl.len()).expect("size fits");
        copy_into(&mut image, 0x24C, &size_bytes.to_le_bytes());

        let dialogs = parse_dialogs(&image, &sections);
        assert_eq!(dialogs.len(), 1);
        let d = &dialogs[0];
        assert_eq!(d.name, 100);
        assert_eq!(d.title, "Dlg");
        assert_eq!((d.cx, d.cy), (160, 60));
        assert!(d.font_point.is_none());
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].class, ItemClass::Static);
        assert_eq!(d.items[0].id, 3);
    }

    #[test]
    fn no_resource_section_yields_empty() {
        assert!(parse_dialogs(&[], &[]).is_empty());
        assert!(parse_dialogs(&[0_u8; 64], &[]).is_empty());
    }

    #[test]
    fn truncated_resource_directory_is_not_fatal() {
        // Section map points at an image that ends inside the directory header.
        let sections = vec![fake_rsrc_section()];
        let image = vec![0_u8; 14];
        assert!(parse_dialogs(&image, &sections).is_empty());
    }

    #[test]
    fn garbage_resource_data_is_not_fatal() {
        let sections = vec![fake_rsrc_section()];
        // Directory with a large entry count, but garbage type ids.
        let mut image = vec![0xFF_u8; 0x400];
        put_u16(&mut image, 0); // named
        put_u16(&mut image, 16); // 16 id entries
        assert!(parse_dialogs(&image, &sections).is_empty());
    }
}
