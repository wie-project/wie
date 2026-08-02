use std::sync::{Arc, Mutex};
use wie_winapi::user32::menu::{MenuEntry, MenuRecord};

/// One node of the guest window's menu tree, as mirrored into the macOS bar.
///
/// A leaf item (`MF_STRING`) has empty `children`; a popup item (`MF_POPUP`)
/// carries its submenu handle in `id` and its submenu's items in `children`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuNode {
    /// `MF_STRING` command id, or submenu handle for `MF_POPUP`.
    pub id: u32,
    /// Item text (empty for separators).
    pub title: String,
    /// Submenu items (non-empty only for `MF_POPUP` items).
    pub children: Vec<MenuNode>,
}

/// Cached menu-bar tree for [`GuestHandle::window_menu_items`]: the tree plus
/// the menu handle it was built from.
pub(super) type MenuTreeCache = Arc<Mutex<Option<(u64, Vec<MenuNode>)>>>;

/// Build the menu tree rooted at `menu_handle` from the native menu records:
/// `Item` entries become leaves, `Popup` entries recurse into their submenu,
/// `Separator` entries are skipped.
pub(super) fn build_menu_tree(menus: &[MenuRecord], menu_handle: u64) -> Vec<MenuNode> {
    let menu_handle = wie_winapi::handles::Hmenu::from(menu_handle);
    let Some(record) = menus.iter().find(|m| m.handle == menu_handle) else {
        return Vec::new();
    };
    record
        .items
        .iter()
        .filter_map(|entry| match entry {
            MenuEntry::Item { id, text, .. } => Some(MenuNode {
                id: *id,
                title: text.clone(),
                children: Vec::new(),
            }),
            MenuEntry::Popup { text, submenu } => Some(MenuNode {
                id: u32::try_from(submenu.as_u64()).unwrap_or(0),
                title: text.clone(),
                children: build_menu_tree(menus, submenu.as_u64()),
            }),
            MenuEntry::Separator => None,
        })
        .collect()
}
