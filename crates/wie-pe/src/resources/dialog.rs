//! `RT_DIALOG` template parsing.
//!
//! Parses the standard `DLGTEMPLATE`/`DLGITEMTEMPLATE` layouts (winuser.h),
//! skips `DLGTEMPLATEEX`, and converts dialog units to pixels. Also owns the
//! template structs shared by the whole resource module (`PixelRect`,
//! `ItemClass`) and the dialog field decoders (`parse_item_class`,
//! `read_optional_text`).

use crate::PeSectionMap;

use super::common::{
    ORDINAL_MARKER, RT_DIALOG, parse_resource_type, read_i16_at, read_u16_at, read_u32_at,
    read_utf16_string,
};

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

/// Class ordinal of the standard `BUTTON` control (winuser.h `WC_BUTTON`).
const CLASS_BUTTON_ORDINAL: u16 = 0x0080;

/// Class ordinal of the standard `EDIT` control (winuser.h `WC_EDIT`).
const CLASS_EDIT_ORDINAL: u16 = 0x0081;

/// Class ordinal of the standard `STATIC` control (winuser.h `WC_STATIC`).
const CLASS_STATIC_ORDINAL: u16 = 0x0082;

/// Class ordinal of the standard `LISTBOX` control (winuser.h `WC_LISTBOX`).
const CLASS_LISTBOX_ORDINAL: u16 = 0x0083;

/// Class ordinal of the standard `COMBOBOX` control (winuser.h `WC_COMBOBOX`).
const CLASS_COMBOBOX_ORDINAL: u16 = 0x0085;

/// High byte of a control-class WORD whose low byte carries the class id
/// (PE spec shorthand for the standard classes).
const CLASS_ID_HIGH_BYTE_MARKER: u16 = 0xFF;

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

/// Parse every `RT_DIALOG` template in `image`.
pub fn parse_dialogs(image: &[u8], sections: &[PeSectionMap]) -> Vec<DialogTemplate> {
    parse_resource_type(image, sections, RT_DIALOG, parse_dialog_template)
}

/// Parse one dialog template from its resource bytes.
///
/// Handles both the standard `DLGTEMPLATE` (winuser.h, style first) and the
/// extended `DLGTEMPLATEEX` (`dlgVer=1`, signature `0xFFFF` — the format
/// `DIALOGEX`/`windres` emit). Malformed templates (items running past the
/// byte slice) are treated as absent.
fn parse_dialog_template(template_id: u16, lang: u16, bytes: &[u8]) -> Option<DialogTemplate> {
    let word0 = read_u16_at(bytes, 0)?;
    let word1 = read_u16_at(bytes, 2)?;

    // DLGTEMPLATEEX (dlgVer=1, signature=0xFFFF): extended layout.
    if word0 == 1 && word1 == ORDINAL_MARKER {
        return parse_dialog_template_ex(template_id, lang, bytes);
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

/// Parse one `DLGTEMPLATEEX` — the extended dialog template `DIALOGEX`
/// resources use (`dlgVer=1`, signature `0xFFFF`; emitted by windres/rc for
/// the `DIALOGEX` statement).
///
/// Header layout (mirroring the winuser.h struct, byte-for-byte as windres
/// emits it): `WORD dlgVer, WORD signature, DWORD helpID, DWORD exStyle,
/// DWORD style, WORD cDlgItems, SHORT x, SHORT y, SHORT cx, SHORT cy`, then
/// the menu/class/title `sz_Or_Ord` fields (packed — binutils windres does
/// NOT DWORD-align these), then — when `DS_SETFONT` — `WORD pointSize, WORD
/// weight, BYTE italic, BYTE charset, WCHAR typeface[]`, then the items.
fn parse_dialog_template_ex(template_id: u16, lang: u16, bytes: &[u8]) -> Option<DialogTemplate> {
    let ex_style = WindowExStyle(read_u32_at(bytes, 8)?);
    let style = WindowStyle(read_u32_at(bytes, 12)?);
    let item_count = u32::from(read_u16_at(bytes, 16)?);
    let x = read_i16_at(bytes, 18)?;
    let y = read_i16_at(bytes, 20)?;
    let cx = read_i16_at(bytes, 22)?;
    let cy = read_i16_at(bytes, 24)?;
    let mut p = 26;

    let (_menu, next) = read_optional_text(bytes, p)?;
    p = next;
    let (_class, next) = read_optional_text(bytes, p)?;
    p = next;
    let (title, next) = read_optional_text(bytes, p)?;
    p = next;

    // DS_SETFONT: the extended template carries pointSize + weight (u16s),
    // italic + charset (u8s), then the NUL-terminated typeface string.
    let mut font_point = None;
    let mut font_face = None;
    if style.contains(WindowStyle::DS_SETFONT) {
        font_point = Some(read_u16_at(bytes, p)?);
        let (face, next) = read_utf16_string(bytes, p.checked_add(6)?, None)?;
        font_face = Some(face);
        p = next;
    }

    // Items are DWORD-aligned relative to the start of the template.
    let mut item_pos = p;
    let mut items = Vec::new();
    for _ in 0..item_count {
        item_pos = align4(item_pos)?;
        let (item, next) = parse_dialog_item_ex(bytes, item_pos)?;
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

/// Parse one `DLGITEMTEMPLATEEX` — the extended item layout `DIALOGEX`
/// templates use — returning the item and the offset just past its creation
/// data.
///
/// Layout (winuser.h, as windres emits it): `DWORD helpID, DWORD exStyle,
/// DWORD style, SHORT x, SHORT y, SHORT cx, SHORT cy, DWORD id`, then the
/// class/title `sz_Or_Ord` fields and the creation-data size word.
fn parse_dialog_item_ex(bytes: &[u8], pos: usize) -> Option<(DialogItemTemplate, usize)> {
    let _help_id = read_u32_at(bytes, pos)?;
    let ex_style = WindowExStyle(read_u32_at(bytes, pos.checked_add(4)?)?);
    let style = WindowStyle(read_u32_at(bytes, pos.checked_add(8)?)?);
    let x = read_i16_at(bytes, pos.checked_add(12)?)?;
    let y = read_i16_at(bytes, pos.checked_add(14)?)?;
    let cx = read_i16_at(bytes, pos.checked_add(16)?)?;
    let cy = read_i16_at(bytes, pos.checked_add(18)?)?;
    // The EX item id is a DWORD; the runtime's GetDlgItem dispatches control
    // ids through WM_COMMAND's 16-bit low word, so truncation matches the
    // emulated surface (ids beyond 0xFFFF are not addressable by the guest).
    let id = u16::try_from(read_u32_at(bytes, pos.checked_add(20)?)?).unwrap_or(0);
    let mut p = pos.checked_add(24)?;

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
    if word == ORDINAL_MARKER {
        let ordinal = read_u16_at(bytes, pos.checked_add(2)?)?;
        return Some((ItemClass::from_ordinal(ordinal), pos.checked_add(4)?));
    }
    if word >> 8 == CLASS_ID_HIGH_BYTE_MARKER {
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
            CLASS_BUTTON_ORDINAL => Self::Button,
            CLASS_EDIT_ORDINAL => Self::Edit,
            CLASS_STATIC_ORDINAL => Self::Static,
            CLASS_LISTBOX_ORDINAL => Self::ListBox,
            CLASS_COMBOBOX_ORDINAL => Self::ComboBox,
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
    if word == ORDINAL_MARKER {
        let _ordinal = read_u16_at(bytes, pos.checked_add(2)?)?;
        return Some((String::new(), pos.checked_add(4)?));
    }
    read_utf16_string(bytes, pos, Some(word))
}

/// Round `pos` up to the next 4-byte boundary.
fn align4(pos: usize) -> Option<usize> {
    pos.checked_add(3).map(|p| p & !3)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::common::test_util::*;
    use super::*;

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
    fn parses_dlg_template_ex() {
        // A `DIALOGEX` template as windres emits it: dlgVer=1, signature
        // 0xFFFF, helpID, exStyle, style (DS_SETFONT), cDlgItems, x/y/cx/cy,
        // then packed menu/class/title, the extended font block
        // (pointSize + weight + italic + charset), then DWORD-aligned items.
        let mut b = Vec::new();
        put_u16(&mut b, 1); // dlgVer
        put_u16(&mut b, 0xFFFF); // signature
        put_u32(&mut b, 0); // helpID
        put_u32(&mut b, 0); // exStyle
        put_u32(&mut b, 0x80C0_0040); // WS_POPUP|WS_CAPTION|DS_SETFONT
        put_u16(&mut b, 1); // cDlgItems
        put_u16(&mut b, 10);
        put_u16(&mut b, 20);
        put_u16(&mut b, 100);
        put_u16(&mut b, 40);
        put_u16(&mut b, 0); // menu: absent
        put_u16(&mut b, 0); // class: absent
        put_utf16(&mut b, "Hi");
        put_u16(&mut b, 8); // pointSize
        put_u16(&mut b, 400); // weight (FW_NORMAL)
        put_u16(&mut b, 0x0100); // italic=0, charset=DEFAULT_CHARSET(1)
        put_utf16(&mut b, "Arial");
        while b.len() & 3 != 0 {
            b.push(0);
        }
        // Item: EDIT at (5, 5, 50, 14), id 0x208, helpID 7.
        put_u32(&mut b, 7); // item helpID
        put_u32(&mut b, 0); // exStyle
        put_u32(&mut b, 0x5000_0080); // WS_CHILD|WS_VISIBLE|ES_AUTOHSCROLL
        put_u16(&mut b, 5);
        put_u16(&mut b, 5);
        put_u16(&mut b, 50);
        put_u16(&mut b, 14);
        put_u32(&mut b, 0x208); // DWORD id
        put_u16(&mut b, 0xFFFF);
        put_u16(&mut b, 0x0081); // EDIT
        put_utf16(&mut b, "");
        put_u16(&mut b, 0); // creation data size

        let t = parse_dialog_template(0x207, 0x0409, &b).expect("template");
        assert_eq!(t.name, 0x207);
        assert_eq!(t.style, 0x80C0_0040);
        assert_eq!((t.x, t.y, t.cx, t.cy), (10, 20, 100, 40));
        assert_eq!(t.pixel_rect.cx, 200);
        assert_eq!(t.title, "Hi");
        assert_eq!(t.font_point, Some(8));
        assert_eq!(t.font_face.as_deref(), Some("Arial"));
        assert_eq!(t.items.len(), 1);
        let item = &t.items[0];
        assert_eq!(item.id, 0x208);
        assert_eq!(item.class, ItemClass::Edit);
        assert_eq!(item.title, "");
        assert_eq!((item.x, item.y, item.cx, item.cy), (5, 5, 50, 14));
        assert_eq!(item.pixel_rect.cx, 100);
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

    #[test]
    fn truncated_dlg_template_ex_is_not_fatal() {
        // EX header claims 1 item but the bytes end inside the header.
        let mut b = Vec::new();
        put_u16(&mut b, 1); // dlgVer
        put_u16(&mut b, 0xFFFF); // signature
        put_u32(&mut b, 0); // helpID
        put_u32(&mut b, 0); // exStyle
        put_u32(&mut b, 0x80C0_0000);
        put_u16(&mut b, 1); // cDlgItems
        assert!(parse_dialog_template(1, 0x0409, &b).is_none());
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
