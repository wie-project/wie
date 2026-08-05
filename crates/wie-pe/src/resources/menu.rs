//! `RT_MENU` template parsing.
//!
//! Parses the classic `MENUITEMTEMPLATE` entry list (winuser.h, as emitted by
//! `windres`): a leading zero DWORD header, then packed, non-aligned entries.

use crate::PeSectionMap;

use super::common::{RT_MENU, parse_resource_type, read_u16_at, read_utf16_string};

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

/// Parse every `RT_MENU` template in `image`.
pub fn parse_menus(image: &[u8], sections: &[PeSectionMap]) -> Vec<MenuTemplate> {
    parse_resource_type(image, sections, RT_MENU, parse_menu_template)
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::common::test_util::*;
    use super::*;

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
}
