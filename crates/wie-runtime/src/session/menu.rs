//! Menu-tree building for the macOS bar (`MenuNode` + build/cache logic).

use std::sync::{Arc, RwLock};
use wie_winapi::handles::Hmenu;
use wie_winapi::user32::menu::{MenuEntry, MenuRecord};

/// One node of the guest window's menu tree, as mirrored into the macOS bar.
///
/// A leaf item (`MF_STRING`) has empty `children`; a popup item (`MF_POPUP`)
/// carries its submenu handle in `id` and its submenu's items in `children`;
/// a separator (`MF_SEPARATOR`) sets `separator` and carries no other state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuNode {
    /// `MF_STRING` command id, or submenu handle for `MF_POPUP`.
    pub id: u32,
    /// Item text (empty for separators), with any `\t`-separated shortcut
    /// suffix already split into [`Self::accelerator`].
    pub title: String,
    /// Whether the item is enabled (`MF_GRAYED`/`MF_DISABLED` clear it).
    pub enabled: bool,
    /// Whether the item is checked (`MF_CHECKED`).
    pub checked: bool,
    /// `MF_SEPARATOR` — the bar renders a separator line instead of a leaf.
    pub separator: bool,
    /// The raw Windows-style shortcut suffix split off `title` at its first
    /// tab (e.g. `"Ctrl+N"`, `"Del"`, `"F5"`); `None` when the item has none.
    ///
    /// Kept as the raw resource string — the host bar (wie) parses it
    /// into a platform key equivalent, since only that crate depends on muda.
    pub accelerator: Option<String>,
    /// Submenu items (non-empty only for `MF_POPUP` items).
    pub children: Vec<MenuNode>,
}

/// Split a guest menu text at its first tab into the display title (which
/// keeps the `&` mnemonic) and the Windows-style shortcut suffix, if any.
///
/// The parsed resource strings keep the literal tab the rc compiler emitted
/// (`"&New\tCtrl+N"` → tab `0x09`), so the host splits without touching the
/// winapi parsing layer.
fn split_accelerator(text: &str) -> (String, Option<String>) {
    match text.split_once('\t') {
        Some((title, suffix)) => {
            let suffix = (!suffix.is_empty()).then(|| suffix.to_owned());
            (title.to_owned(), suffix)
        }
        None => (text.to_owned(), None),
    }
}

/// Cached menu-bar tree for [`GuestHandle::window_menu_items`]: the tree plus
/// the menu handle it was built from.
///
/// `RwLock`: the host frame loop is the sole reader (read lock on the GUI
/// path); only a `menu_dirty` rebuild takes the write lock. The tree is
/// shared as an `Arc` so the per-frame cache-hit path clones a refcounted
/// pointer instead of deep-cloning every `MenuNode` (String titles + child
/// Vecs) once per frame.
pub(super) type MenuTreeCache = Arc<RwLock<Option<(Hmenu, Arc<Vec<MenuNode>>)>>>;

/// Build the menu tree rooted at `menu_handle` from the native menu records:
/// `Item` entries become leaves (with any `\t` shortcut suffix split off),
/// `Popup` entries recurse into their submenu, `Separator` entries become
/// separator nodes the host bar renders as real separator lines.
pub(super) fn build_menu_tree(menus: &[MenuRecord], menu_handle: u64) -> Vec<MenuNode> {
    let menu_handle = wie_winapi::handles::Hmenu::from(menu_handle);
    let Some(record) = menus.iter().find(|m| m.handle == menu_handle) else {
        return Vec::new();
    };
    record
        .items
        .iter()
        .map(|entry| match entry {
            MenuEntry::Item {
                id,
                text,
                enabled,
                checked,
                ..
            } => {
                let (title, accelerator) = split_accelerator(text);
                MenuNode {
                    id: *id,
                    title,
                    enabled: *enabled,
                    checked: *checked,
                    separator: false,
                    accelerator,
                    children: Vec::new(),
                }
            }
            MenuEntry::Popup { text, submenu } => {
                let (title, _) = split_accelerator(text);
                MenuNode {
                    id: u32::try_from(submenu.as_u64()).unwrap_or(0),
                    title,
                    // Popups carry no state in the native model (only leaves
                    // are enableable/checkable), so the mirror defaults them.
                    enabled: true,
                    checked: false,
                    separator: false,
                    // macOS top-level menus render no key equivalent, so the
                    // suffix (if any) is stripped from the title and dropped.
                    accelerator: None,
                    children: build_menu_tree(menus, submenu.as_u64()),
                }
            }
            MenuEntry::Separator => MenuNode {
                id: 0,
                title: String::new(),
                enabled: true,
                checked: false,
                separator: true,
                accelerator: None,
                children: Vec::new(),
            },
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{build_menu_tree, split_accelerator};
    use wie_winapi::handles::Hmenu;
    use wie_winapi::user32::menu::{MenuEntry, MenuRecord};

    fn record(handle: u64, items: Vec<MenuEntry>) -> MenuRecord {
        MenuRecord {
            handle: Hmenu::from(handle),
            items,
        }
    }

    /// The `\t` shortcut suffix is split off the display title and carried
    /// as the raw accelerator string; titles without a tab pass through.
    #[test]
    fn split_accelerator_separates_tab_suffix() {
        assert_eq!(
            split_accelerator("&New\tCtrl+N"),
            ("&New".to_owned(), Some("Ctrl+N".to_owned()))
        );
        assert_eq!(
            split_accelerator("De&lete\tDel"),
            ("De&lete".to_owned(), Some("Del".to_owned()))
        );
        assert_eq!(
            split_accelerator("&Word Wrap"),
            ("&Word Wrap".to_owned(), None)
        );
        // A trailing tab yields no accelerator, only the trimmed title.
        assert_eq!(split_accelerator("&About\t"), ("&About".to_owned(), None));
    }

    /// `build_menu_tree` keeps `MF_SEPARATOR` entries as separator nodes (the
    /// host bar turns them into real separator lines) and strips the
    /// shortcut suffix off item titles.
    #[test]
    fn build_menu_tree_keeps_separators_and_strips_shortcuts() {
        let menus = vec![record(
            0x6620_0000,
            vec![
                MenuEntry::Item {
                    id: 100,
                    text: "&New\tCtrl+N".to_owned(),
                    enabled: true,
                    checked: false,
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    id: 101,
                    text: "E&xit".to_owned(),
                    enabled: true,
                    checked: false,
                },
            ],
        )];
        let tree = build_menu_tree(&menus, 0x6620_0000);
        assert_eq!(tree.len(), 3, "separator is no longer dropped");
        let new_item = tree.first().expect("item");
        assert_eq!(
            (new_item.title.as_str(), new_item.accelerator.as_deref()),
            ("&New", Some("Ctrl+N")),
            "item title is stripped of the shortcut suffix"
        );
        assert!(
            tree.get(1).expect("separator").separator,
            "separator entry becomes a separator node"
        );
        let exit = tree.get(2).expect("item");
        assert!(!exit.separator && exit.accelerator.is_none());
    }
}
