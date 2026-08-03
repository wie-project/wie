//! RT_DIALOG, RT_MENU, and RT_STRING resource parsing for PE images.
//!
//! Walks the `IMAGE_RESOURCE_DIRECTORY` tree in the `.rsrc` section (PE
//! resource format, Microsoft Learn) and parses each `RT_DIALOG` template
//! (type **5** — note: 16 is `RT_VERSION`) into a [`DialogTemplate`], each
//! `RT_MENU` template (type **4**) into a [`MenuTemplate`], and each
//! `RT_STRING` block (type **6**) into a [`StringBlock`].
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

/// Type id of `RT_MENU` resources in the resource directory
/// (`MAKEINTRESOURCE(4)`). `RT_MENUEX` (11) is the extended variant and is
/// not parsed.
const RT_MENU: u16 = 4;

/// Type id of `RT_STRING` resources in the resource directory
/// (`MAKEINTRESOURCE(6)`).
const RT_STRING: u16 = 6;

/// Type id of `RT_ACCELERATOR` resources in the resource directory
/// (`MAKEINTRESOURCE(9)`).
const RT_ACCELERATOR: u16 = 9;

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

/// Safety cap: dialog/menu/string-table strings arrive as length-prefixed
/// bytes from a possibly hostile PE, and a corrupt prefix or a missing NUL
/// terminator must not force unbounded parse-time allocation. 4096 UTF-16
/// words (8 KiB) bounds the scan while sitting far above any realistic field
/// string.
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
    /// Language id of the block this template came from (the language-level
    /// directory entry key, e.g. `0x0409` en-US); `0` when the resource tree
    /// has no language level.
    pub lang: u16,
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

/// One parsed `RT_MENU` template (classic `MENUITEMTEMPLATE` list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuTemplate {
    /// Template id (the resource name at the second directory level); named
    /// resources have no numeric id and come back as `0` (unaddressable by
    /// `LoadMenuW`, which resolves ids only)
    pub id: u32,
    /// Language id of the block this template came from (the language-level
    /// directory entry key, e.g. `0x0409` en-US); `0` when the resource tree
    /// has no language level.
    pub lang: u16,
    /// Top-level items in template order (always popups on a menu bar)
    pub items: Vec<MenuItemTemplate>,
}

/// One parsed `MENUITEMTEMPLATE` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItemTemplate {
    /// Raw `mtOption` flags (`MF_*`), including `MF_POPUP`, `MF_SEPARATOR`,
    /// and `MF_END`.
    pub flags: u32,
    /// Command id (`mtID`); `0` for popups and separators.
    pub id: u32,
    /// Item text: the popup title for `MF_POPUP` items, the label otherwise.
    /// Separators have no text (`None`).
    pub text: Option<String>,
    /// Popup entries (popups only, in template order).
    pub sub: Vec<MenuItemTemplate>,
}

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

/// `MF_*` menu option flags (winuser.h) kept on [`MenuItemTemplate::flags`].
///
/// Only the bits this parser understands are named; unknown bits pass through
/// unchanged so consumers can inspect the raw resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuFlags(u32);

impl MenuFlags {
    /// `MF_POPUP`: the entry opens a submenu; its string is the popup title
    /// and the entries that follow belong to the popup.
    pub(crate) const POPUP: Self = Self(0x10);

    /// `MF_END`: the last entry of its level. Terminates the current popup's
    /// entry list (or the top-level bar) during parsing.
    pub(crate) const END: Self = Self(0x80);

    /// `MF_SEPARATOR`: a separator line, no id or text.
    pub(crate) const SEPARATOR: Self = Self(0x800);

    /// Raw flag bits.
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
fn parse_resource_type<T>(
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

/// Parse every `RT_DIALOG` template in `image`.
pub fn parse_dialogs(image: &[u8], sections: &[PeSectionMap]) -> Vec<DialogTemplate> {
    parse_resource_type(image, sections, RT_DIALOG, parse_dialog_template)
}

/// Parse every `RT_MENU` template in `image`.
pub fn parse_menus(image: &[u8], sections: &[PeSectionMap]) -> Vec<MenuTemplate> {
    parse_resource_type(image, sections, RT_MENU, parse_menu_template)
}

/// Parse every `RT_STRING` block in `image`.
pub fn parse_strings(image: &[u8], sections: &[PeSectionMap]) -> Vec<StringBlock> {
    parse_resource_type(image, sections, RT_STRING, parse_string_block)
}

/// Parse every `RT_ACCELERATOR` table in `image`.
pub fn parse_accelerators(image: &[u8], sections: &[PeSectionMap]) -> Vec<AccelTemplate> {
    parse_resource_type(image, sections, RT_ACCELERATOR, parse_accel_table)
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

/// Parse one dialog template from its resource bytes.
///
/// Skips `DLGTEMPLATEEX` and any template whose items run past the
/// byte slice (malformed → treated as absent).
fn parse_dialog_template(template_id: u16, lang: u16, bytes: &[u8]) -> Option<DialogTemplate> {
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
        lang,
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

/// Parse one classic `RT_MENU` template from its resource bytes.
///
/// Layout (winuser.h `MENUITEMTEMPLATE`, as emitted by `windres`): a leading
/// zero DWORD header, then packed entries. Each entry is `WORD mtOption`,
/// then — for `MF_POPUP` entries — the null-terminated title string directly
/// (no `mtID`), or — otherwise — `WORD mtID` followed by the null-terminated
/// text. Popup entries are followed by their sub-entries; an entry whose
/// option carries `MF_END` (0x80) is the last of its level. Entries are NOT
/// DWORD-aligned in this format (unlike dialog items): each entry starts
/// immediately after the previous string's terminator.
fn parse_menu_template(template_id: u16, lang: u16, bytes: &[u8]) -> Option<MenuTemplate> {
    // `windres` prefixes the entry list with a zero version/header DWORD.
    let header = u32::from(read_u16_at(bytes, 0)?) | (u32::from(read_u16_at(bytes, 2)?) << 16);
    let mut pos = if header == 0 { 4 } else { 0 };
    let items = parse_menu_entries(bytes, &mut pos, true)?;
    Some(MenuTemplate {
        id: u32::from(template_id),
        lang,
        items,
    })
}

/// Parse the entries of one menu level.
///
/// Reads entries until an `MF_END` entry (the last of its level, per
/// `MENUITEMTEMPLATE`). A nested level that runs past `bytes` fails (the
/// whole template is treated as absent, like malformed dialog data); the top
/// level tolerates a missing terminator by accepting what parsed.
fn parse_menu_entries(
    bytes: &[u8],
    pos: &mut usize,
    top_level: bool,
) -> Option<Vec<MenuItemTemplate>> {
    let mut items = Vec::new();
    loop {
        let entry = parse_menu_entry(bytes, pos)?;
        let is_last = entry.flags & MenuFlags::END.bits() != 0;
        items.push(entry);
        if is_last {
            break;
        }
        if *pos >= bytes.len() {
            // Unterminated level. Accept the partial list only at the top
            // level; a popup body must close with MF_END.
            if top_level {
                break;
            }
            return None;
        }
    }
    Some(items)
}

/// Parse one `MENUITEMTEMPLATE` entry, consuming its sub-entries when it is a
/// popup.
fn parse_menu_entry(bytes: &[u8], pos: &mut usize) -> Option<MenuItemTemplate> {
    let option = u32::from(read_u16_at(bytes, *pos)?);
    *pos = pos.checked_add(2)?;
    let flags = MenuFlags(option);
    let (id, text) = if flags.contains(MenuFlags::POPUP) {
        // Popup: no mtID; the title string follows the option word.
        let (title, next) = read_utf16_string(bytes, *pos, None)?;
        *pos = next;
        (0, Some(title))
    } else {
        let id = u32::from(read_u16_at(bytes, *pos)?);
        *pos = pos.checked_add(2)?;
        let (text, next) = read_utf16_string(bytes, *pos, None)?;
        *pos = next;
        // A separator is an empty-string entry; `windres` emits it with a
        // zero option word (id 0, no text), not with the MF_SEPARATOR bit.
        let text = if flags.contains(MenuFlags::SEPARATOR) || text.is_empty() {
            None
        } else {
            Some(text)
        };
        (id, text)
    };
    let mut sub = Vec::new();
    if flags.contains(MenuFlags::POPUP) {
        sub = parse_menu_entries(bytes, pos, false)?;
    }
    Some(MenuItemTemplate {
        flags: option,
        id,
        text,
        sub,
    })
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
    while let Some(entry) = bytes.get(pos..pos.checked_add(6)?) {
        // `entry` is exactly 6 bytes, so every field read below succeeds.
        let flags = read_u16_at(entry, 0)?;
        let key = read_u16_at(entry, 2)?;
        let command_id = read_u16_at(entry, 4)?;
        entries.push(AccelEntry {
            flags,
            key,
            command_id,
        });
        pos = pos.checked_add(6)?;
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

        let t = parse_dialog_template(42, 0x0409, &b).expect("template");
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

        let t = parse_dialog_template(7, 0x0409, &b).expect("msdn variant");
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
        assert!(parse_dialog_template(1, 0x0409, &bytes).is_none());
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
        assert!(parse_dialog_template(1, 0x0409, &b).is_none());
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

    /// Build the classic `MENUITEMTEMPLATE` bytes for one entry.
    ///
    /// `popup` selects the no-`mtID` layout (`option`, then title string);
    /// otherwise the entry is `option`, `id`, then text.
    fn menu_entry(popup: bool, option: u16, id: u16, text: &str) -> Vec<u8> {
        let mut b = Vec::new();
        put_u16(&mut b, option);
        if !popup {
            put_u16(&mut b, id);
        }
        put_utf16(&mut b, text);
        b
    }

    #[test]
    fn parses_classic_menu_template() {
        // windres layout: zero DWORD header, then packed entries.
        let mut b = Vec::new();
        put_u32(&mut b, 0); // header
        // "&File" popup (MF_POPUP, no MF_END — more popups follow).
        b.extend(menu_entry(true, 0x0010, 0, "&File"));
        b.extend(menu_entry(false, 0x0000, 0x0100, "&New\tCtrl+N"));
        b.extend(menu_entry(false, 0x0000, 0x0103, "Save &As..."));
        // Separator: windres emits option 0 + empty string, no MF_SEPARATOR.
        b.extend(menu_entry(false, 0x0000, 0x0000, ""));
        // Last File entry: MF_END (0x80) terminates the popup body.
        b.extend(menu_entry(false, 0x0080, 0x0108, "E&xit"));
        // "&Edit" popup with MF_END: the bar's last entry.
        b.extend(menu_entry(true, 0x0090, 0, "&Edit"));
        b.extend(menu_entry(false, 0x0000, 0x0110, "&Undo\tCtrl+Z"));
        b.extend(menu_entry(false, 0x0080, 0x0117, "Time/&Date\tF5"));

        let m = parse_menu_template(0x201, 0x0409, &b).expect("template");
        assert_eq!(m.id, 0x201);
        assert_eq!(m.items.len(), 2);

        let file = &m.items[0];
        assert_eq!(
            file.flags & MenuFlags::POPUP.bits(),
            MenuFlags::POPUP.bits()
        );
        assert_eq!(file.text.as_deref(), Some("&File"));
        assert_eq!(file.id, 0);
        assert_eq!(file.sub.len(), 4);
        assert_eq!(file.sub[0].id, 0x0100);
        assert_eq!(file.sub[0].text.as_deref(), Some("&New\tCtrl+N"));
        assert_eq!(file.sub[0].flags & MenuFlags::END.bits(), 0);
        assert_eq!(file.sub[1].id, 0x0103);
        assert_eq!(file.sub[1].text.as_deref(), Some("Save &As..."));
        // Separator: no text, id 0.
        assert_eq!(file.sub[2].id, 0);
        assert_eq!(file.sub[2].text, None);
        // MF_END entry is included in the popup body.
        assert_eq!(file.sub[3].id, 0x0108);
        assert_eq!(file.sub[3].text.as_deref(), Some("E&xit"));
        assert_ne!(file.sub[3].flags & MenuFlags::END.bits(), 0);

        let edit = &m.items[1];
        assert_ne!(edit.flags & MenuFlags::END.bits(), 0);
        assert_eq!(edit.text.as_deref(), Some("&Edit"));
        assert_eq!(edit.sub.len(), 2);
        assert_eq!(edit.sub[1].id, 0x0117);
        assert_ne!(edit.sub[1].flags & MenuFlags::END.bits(), 0);
    }

    #[test]
    fn parses_menu_separator_with_flag() {
        // Some toolchains emit MF_SEPARATOR (0x800) explicitly.
        let mut b = Vec::new();
        put_u32(&mut b, 0); // header
        b.extend(menu_entry(true, 0x0090, 0, "&File"));
        b.extend(menu_entry(false, 0x0800, 0, ""));
        b.extend(menu_entry(false, 0x0080, 7, "Item"));

        let m = parse_menu_template(9, 0x0409, &b).expect("template");
        let file = &m.items[0];
        assert_eq!(file.sub.len(), 2);
        assert_eq!(file.sub[0].text, None);
        assert_ne!(file.sub[0].flags & MenuFlags::SEPARATOR.bits(), 0);
        assert_eq!(file.sub[1].id, 7);
        assert_eq!(file.sub[1].text.as_deref(), Some("Item"));
    }

    #[test]
    fn menu_without_header_is_accepted() {
        // No leading zero DWORD: entries start immediately.
        let mut b = Vec::new();
        b.extend(menu_entry(true, 0x0010, 0, "&File"));
        b.extend(menu_entry(false, 0x0080, 3, "E&xit"));

        let m = parse_menu_template(1, 0x0409, &b).expect("template");
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].sub.len(), 1);
        assert_eq!(m.items[0].sub[0].id, 3);
    }

    #[test]
    fn truncated_menu_template_is_not_fatal() {
        // Popup body never closes (no MF_END, bytes end inside it).
        let mut b = Vec::new();
        put_u32(&mut b, 0);
        b.extend(menu_entry(true, 0x0090, 0, "&File"));
        b.extend(menu_entry(false, 0x0000, 1, "A"));
        assert!(parse_menu_template(1, 0x0409, &b).is_none());
        // Missing bytes entirely.
        assert!(parse_menu_template(1, 0x0409, &[]).is_none());
    }

    #[test]
    fn parses_menu_from_synthetic_resource_tree() {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1200];

        // Root dir @0x200: type RT_MENU → subdir 0x210.
        let mut root = Vec::new();
        put_u32(&mut root, 0);
        put_u32(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 0);
        put_u16(&mut root, 1);
        put_u32(&mut root, u32::from(RT_MENU));
        put_u32(&mut root, 0x8000_0018);
        copy_into(&mut image, 0x200, &root);

        // Type dir @0x218: menu id 0x201 → subdir 0x230.
        let mut type_dir = Vec::new();
        put_u32(&mut type_dir, 0);
        put_u32(&mut type_dir, 0);
        put_u16(&mut type_dir, 0);
        put_u16(&mut type_dir, 0);
        put_u16(&mut type_dir, 0);
        put_u16(&mut type_dir, 1);
        put_u32(&mut type_dir, 0x0201);
        put_u32(&mut type_dir, 0x8000_0030);
        copy_into(&mut image, 0x218, &type_dir);

        // Language dir @0x230: language 0x409 → data entry 0x248.
        let mut lang = Vec::new();
        put_u32(&mut lang, 0);
        put_u32(&mut lang, 0);
        put_u16(&mut lang, 0);
        put_u16(&mut lang, 0);
        put_u16(&mut lang, 0);
        put_u16(&mut lang, 1);
        put_u32(&mut lang, 0x0409);
        put_u32(&mut lang, 0x48);
        copy_into(&mut image, 0x230, &lang);

        // Data entry @0x248: template at rva 0x1200 → file 0x400.
        let mut data = Vec::new();
        put_u32(&mut data, 0x1200);
        put_u32(&mut data, 0);
        put_u32(&mut data, 0);
        put_u32(&mut data, 0);
        copy_into(&mut image, 0x248, &data);

        // Template @0x400: bar with one popup and one item.
        let mut tpl = Vec::new();
        put_u32(&mut tpl, 0); // header
        tpl.extend(menu_entry(true, 0x0010, 0, "&File"));
        tpl.extend(menu_entry(false, 0x0080, 0x0123, "&Go To...\tCtrl+G"));
        copy_into(&mut image, 0x400, &tpl);
        let size_bytes = u32::try_from(tpl.len()).expect("size fits");
        copy_into(&mut image, 0x24C, &size_bytes.to_le_bytes());

        let menus = parse_menus(&image, &sections);
        assert_eq!(menus.len(), 1);
        let m = &menus[0];
        assert_eq!(m.id, 0x201);
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].text.as_deref(), Some("&File"));
        assert_eq!(m.items[0].sub.len(), 1);
        assert_eq!(m.items[0].sub[0].id, 0x0123);
        assert_eq!(m.items[0].sub[0].text.as_deref(), Some("&Go To...\tCtrl+G"));
    }

    #[test]
    fn no_menu_resource_yields_empty() {
        assert!(parse_menus(&[], &[]).is_empty());
        assert!(parse_menus(&[0_u8; 64], &[]).is_empty());
    }

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

    /// Emit an `IMAGE_RESOURCE_DIRECTORY` header with a single id entry.
    fn push_dir_entry(buf: &mut Vec<u8>, id: u32, offset: u32) {
        put_u32(buf, 0); // characteristics
        put_u32(buf, 0); // timestamp
        put_u16(buf, 0); // major version
        put_u16(buf, 0); // minor version
        put_u16(buf, 0); // named entries
        put_u16(buf, 1); // id entries
        put_u32(buf, id);
        put_u32(buf, offset);
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
