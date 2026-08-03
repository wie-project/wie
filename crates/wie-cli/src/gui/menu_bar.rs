//! Safe bridge from the guest's window menu to the macOS application menu bar.
//!
//! Built on the `muda` crate, which wraps AppKit behind a safe API — this
//! module contains no raw Objective-C (no `unsafe`, no `declare_class!`, no
//! `msg_send!`). muda objects may only be touched on the main thread (muda
//! panics if a [`Menu`] is created off it), so every method here checks the
//! thread [`MacMenuBar::new`] ran on and no-ops otherwise; the next `Frame`
//! retries.

use std::thread::ThreadId;

use muda::accelerator::Accelerator;
use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use wie_runtime::MenuNode;
use winit::event_loop::EventLoopProxy;

use super::app::WieEvent;

/// Encodes a guest menu item id (`u32`) into muda's string-form [`MenuId`]
/// (muda has no integer id form).
fn guest_id_to_menu_id(id: u32) -> MenuId {
    MenuId::new(id.to_string())
}

/// Decodes a [`MenuId`] back into the guest menu item id.
///
/// Non-numeric ids (defensive — we only ever create numeric ones) decode to 0.
pub fn menu_id_to_guest_id(id: &MenuId) -> u32 {
    id.0.parse::<u32>().unwrap_or(0)
}

/// Parse a Windows-style menu shortcut suffix — the exact forms RNotepad's
/// resources use (`"Ctrl+N"`, `"Ctrl+Shift+N"`, `"F5"`, `"Del"`, plus the
/// German `"Strg+…"`/`"Umschalt"` and Turkish `"Sil"` spellings) — into a
/// muda [`Accelerator`].
///
/// Windows menu strings label the primary shortcut "Ctrl" while modern macOS
/// apps use Command, so `Ctrl`/`Strg` normalize to muda's `CmdOrCtrl` token
/// (Command on macOS, Control elsewhere — the
/// [`muda::accelerator::CMD_OR_CTRL`] convention). Localized delete labels
/// (`Del`/`Entf`/`Sil`) map to the Delete key. Anything outside that grammar
/// — or muda cannot parse — falls back to `None`, and the native item is
/// simply left without a key equivalent.
fn parse_menu_accelerator(suffix: &str) -> Option<Accelerator> {
    let normalized = normalize_accelerator_tokens(suffix)?;
    normalized.parse::<Accelerator>().ok()
}

/// Rewrite a guest shortcut suffix into the token spelling muda understands.
fn normalize_accelerator_tokens(suffix: &str) -> Option<String> {
    let tokens: Vec<&str> = suffix.split('+').collect();
    let (key, modifiers) = tokens.split_last()?;
    let mut normalized: Vec<String> = Vec::new();
    for token in modifiers {
        match token.to_ascii_uppercase().as_str() {
            "CTRL" | "STRG" => normalized.push("CmdOrCtrl".to_owned()),
            "SHIFT" | "UMSCHALT" => normalized.push("Shift".to_owned()),
            "ALT" => normalized.push("Alt".to_owned()),
            _ => return None, // unknown modifier — do not guess
        }
    }
    let key = match key.to_ascii_uppercase().as_str() {
        "DEL" | "ENTF" | "SIL" => "Delete".to_owned(),
        other => other.to_owned(),
    };
    normalized.push(key);
    Some(normalized.join("+"))
}

/// The muda state one leaf menu item must reflect: its guest command id plus
/// the enabled/checked flags taken from the `MenuNode` tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItemState {
    /// Guest command id — the `MenuId` the bar stamps the item with, so
    /// states match items by their id path.
    pub id: u32,
    /// `false` when the guest greyed the item (`MF_GRAYED`/`MF_DISABLED`).
    pub enabled: bool,
    /// `true` when the guest checked the item (`MF_CHECKED`).
    pub checked: bool,
}

/// Map a `MenuNode` tree onto the per-leaf muda states, in the same
/// depth-first order [`MacMenuBar::rebuild`] appends items to the bar.
///
/// This is the pure "node → host state" half of the menu-state round trip:
/// the guest's `EnableMenuItem`/`CheckMenuItem` calls flip the `MenuNode`
/// flags (wie-runtime), and this walk turns them into the states the bar
/// applies via muda's `set_enabled`/`set_checked`. Being pure, it is unit
/// testable without a live muda bar or window.
pub fn menu_item_states(nodes: &[MenuNode]) -> Vec<MenuItemState> {
    let mut states = Vec::new();
    collect_item_states(nodes, &mut states);
    states
}

/// Depth-first leaf walk shared by [`menu_item_states`] and the bar build,
/// so the state list always lines up with the append order. Separator nodes
/// contribute no state — the bar renders them as lines, not items.
fn collect_item_states(nodes: &[MenuNode], out: &mut Vec<MenuItemState>) {
    for node in nodes {
        if node.separator {
            continue;
        }
        if node.children.is_empty() {
            out.push(MenuItemState {
                id: node.id,
                enabled: node.enabled,
                checked: node.checked,
            });
        } else {
            collect_item_states(&node.children, out);
        }
    }
}

/// A retained leaf menu item of the bar, typed by whether it may show a
/// checkmark.
///
/// Only currently-checked items are created as muda [`CheckMenuItem`]s:
/// muda auto-toggles a check item on click, so the unchecked majority must
/// stay plain [`MenuItem`]s or an ordinary command click would leave a stray
/// checkmark until the next rebuild. The type follows `node.checked` at each
/// rebuild, so a guest toggling an item flips its type on the next bar sync.
enum LeafItem {
    /// No checkmark support (`checked == false` in the tree).
    Plain(MenuItem),
    /// Renders the guest's checked state.
    Checkable(CheckMenuItem),
}

impl LeafItem {
    /// Create the leaf in a default enabled/unchecked state; the real state
    /// is stamped afterwards by [`apply_item_states`] from the pure mapping.
    /// The accelerator is applied at creation (it never changes for a built
    /// item) — the `\t` suffix was already split off `title` by the tree
    /// build and lands here as the muda key equivalent, so AppKit renders
    /// the standard grey right-aligned shortcut instead of white inline text.
    fn new(node: &MenuNode) -> Self {
        let id = guest_id_to_menu_id(node.id);
        let accelerator = node.accelerator.as_deref().and_then(parse_menu_accelerator);
        if node.checked {
            Self::Checkable(CheckMenuItem::with_id(
                id,
                &node.title,
                true,
                false,
                accelerator,
            ))
        } else {
            Self::Plain(MenuItem::with_id(id, &node.title, true, accelerator))
        }
    }

    fn id(&self) -> &MenuId {
        match self {
            Self::Plain(item) => item.id(),
            Self::Checkable(item) => item.id(),
        }
    }

    fn as_item(&self) -> &dyn muda::IsMenuItem {
        match self {
            Self::Plain(item) => item,
            Self::Checkable(item) => item,
        }
    }

    fn set_enabled(&self, enabled: bool) {
        match self {
            Self::Plain(item) => item.set_enabled(enabled),
            Self::Checkable(item) => item.set_enabled(enabled),
        }
    }

    fn set_checked(&self, checked: bool) {
        if let Self::Checkable(item) = self {
            item.set_checked(checked);
        }
    }
}

/// Apply `states` (in bar-append order) to the retained leaf items.
///
/// Each item is matched by its id path — the guest command id stamped into
/// the `MenuId` at creation — so a future ordering drift between the build
/// walk and the state walk surfaces as a warn-and-skip instead of state
/// landing on the wrong item.
fn apply_item_states(leaves: &[LeafItem], states: &[MenuItemState]) {
    for (state, leaf) in states.iter().zip(leaves) {
        if leaf.id().0 != state.id.to_string() {
            tracing::warn!(
                expected_id = state.id,
                actual_id = %leaf.id().0,
                "menu bar item id mismatch while applying state; skipped"
            );
            continue;
        }
        leaf.set_enabled(state.enabled);
        leaf.set_checked(state.checked);
    }
}

/// macOS application menu bar (the top bar), mirroring the guest window menu.
///
/// Not [`Send`]: holds muda objects backed by AppKit, which may only be used
/// on the main thread.
pub struct MacMenuBar {
    /// The installed root menu; `None` until the first successful [`rebuild`].
    ///
    /// [`rebuild`]: MacMenuBar::rebuild
    menu: Option<Menu>,
    /// The thread [`new`] ran on (the winit event-loop thread). muda panics
    /// if a [`Menu`] is created off the main thread, so any other thread's
    /// rebuild is skipped and retried by the next `Frame`.
    ///
    /// [`new`]: MacMenuBar::new
    main_thread: ThreadId,
}

impl MacMenuBar {
    /// Creates the bridge and installs the process-wide muda click handler.
    ///
    /// The handler is registered exactly once, here: every menu click is
    /// forwarded through the winit `proxy` as [`WieEvent::MenuEvent`], so the
    /// guest mutation (`WM_COMMAND`) always runs on the event-loop thread.
    /// This is mutually exclusive with [`MenuEvent::receiver`] — when a
    /// handler is set, muda routes all events to it.
    pub fn new(proxy: EventLoopProxy<WieEvent>) -> Self {
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(WieEvent::MenuEvent(event));
        }));
        Self {
            menu: None,
            main_thread: std::thread::current().id(),
        }
    }

    /// Replaces the menu-bar contents with one top-level macOS menu per
    /// guest top-level menu item.
    ///
    /// AppKit reserves the FIRST main-menu slot for the application menu and
    /// displays it with the app name (the title is ignored), so an "App"
    /// placeholder submenu occupies that slot first; the guest's popup items
    /// (`MF_POPUP`) become real top-level menus titled with their text, and
    /// leaf items become plain top-level commands. The previously installed
    /// menu is removed before the new one is built, so a rebuild never stacks
    /// menus. No-op when called off the main thread (muda would panic); the
    /// next `Frame`'s call retries.
    pub fn rebuild(&mut self, items: &[MenuNode]) {
        if std::thread::current().id() != self.main_thread {
            tracing::warn!("menu bar rebuild off the main thread; skipped");
            return;
        }

        if let Some(menu) = self.menu.take() {
            menu.remove_for_nsapp();
        }

        let menu = Menu::new();
        // App-menu slot: macOS shows the app name here, never our title, so
        // an empty placeholder keeps the guest's own menus titled correctly.
        let app_menu = Submenu::new("App", true);
        if let Err(error) = menu.append(&app_menu) {
            tracing::warn!(error = %error, "muda: failed to append app menu");
        }
        // Leaves are appended in a default state and stamped afterwards from
        // the pure `menu_item_states` mapping, so the guest's enabled/checked
        // state reaches the native items through exactly the code path the
        // unit tests pin (no live bar needed for the mapping itself).
        let mut leaves = Vec::new();
        for node in items {
            if node.separator {
                // A real macOS separator line (the guest's MF_SEPARATOR
                // group), replacing the spurious vertical gap.
                if let Err(error) = menu.append(&PredefinedMenuItem::separator()) {
                    tracing::warn!(error = %error, "muda: failed to append separator");
                }
            } else if node.children.is_empty() {
                // Leaf: a top-level command item.
                let item = LeafItem::new(node);
                if let Err(error) = menu.append(item.as_item()) {
                    tracing::warn!(error = %error, "muda: failed to append menu item");
                }
                leaves.push(item);
            } else {
                // Popup: a top-level menu holding the children.
                let submenu = Submenu::new(&node.title, true);
                append_children(&submenu, &node.children, &mut leaves);
                // AppKit retains the appended items (and the root menu retains
                // the submenu), so the local wrapper may drop after append.
                if let Err(error) = menu.append(&submenu) {
                    tracing::warn!(error = %error, "muda: failed to append submenu");
                }
            }
        }
        apply_item_states(&leaves, &menu_item_states(items));
        menu.init_for_nsapp();
        self.menu = Some(menu);
    }
}

/// Recursively append `nodes` into `submenu` (leaf items, nested popups and
/// separators), retaining every leaf so [`apply_item_states`] can stamp its
/// state after the build — the same depth-first order `menu_item_states`
/// walks (which likewise skips separator nodes).
fn append_children(submenu: &Submenu, nodes: &[MenuNode], leaves: &mut Vec<LeafItem>) {
    for node in nodes {
        if node.separator {
            if let Err(error) = submenu.append(&PredefinedMenuItem::separator()) {
                tracing::warn!(error = %error, "muda: failed to append separator");
            }
        } else if node.children.is_empty() {
            let item = LeafItem::new(node);
            if let Err(error) = submenu.append(item.as_item()) {
                tracing::warn!(error = %error, "muda: failed to append menu item");
            }
            leaves.push(item);
        } else {
            let child = Submenu::new(&node.title, true);
            append_children(&child, &node.children, leaves);
            if let Err(error) = submenu.append(&child) {
                tracing::warn!(error = %error, "muda: failed to append submenu");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn leaf(id: u32, title: &str, enabled: bool, checked: bool) -> MenuNode {
        MenuNode {
            id,
            title: title.to_owned(),
            enabled,
            checked,
            separator: false,
            accelerator: None,
            children: Vec::new(),
        }
    }

    fn separator() -> MenuNode {
        MenuNode {
            id: 0,
            title: String::new(),
            enabled: true,
            checked: false,
            separator: true,
            accelerator: None,
            children: Vec::new(),
        }
    }

    /// `menu_item_states` walks the tree depth-first — the same order the bar
    /// appends items — emitting one state per leaf with the flags from the
    /// node, skipping separator nodes (the bar renders them as lines, so
    /// they carry no state to stamp). This pins the host-side half of the
    /// F4 round trip without any live muda bar.
    #[test]
    fn menu_item_states_maps_leaves_in_bar_order() {
        let nodes = vec![
            MenuNode {
                id: 1,
                title: "File".to_owned(),
                enabled: true,
                checked: false,
                separator: false,
                accelerator: None,
                children: vec![
                    // Greys out, like notepad's Paste when the clipboard is
                    // empty (EnableMenuItem MF_GRAYED|MF_BYCOMMAND).
                    leaf(100, "Paste", false, false),
                    separator(),
                    leaf(101, "Exit", true, false),
                ],
            },
            MenuNode {
                id: 2,
                title: "Format".to_owned(),
                enabled: true,
                checked: false,
                separator: false,
                accelerator: None,
                children: vec![
                    // Checked, like notepad's Word Wrap (CheckMenuItem
                    // MF_CHECKED|MF_BYCOMMAND).
                    leaf(200, "Word Wrap", true, true),
                ],
            },
            // A leaf at the top level appends directly to the root menu.
            leaf(300, "Toggle", true, false),
        ];
        assert_eq!(
            menu_item_states(&nodes),
            vec![
                MenuItemState {
                    id: 100,
                    enabled: false,
                    checked: false
                },
                MenuItemState {
                    id: 101,
                    enabled: true,
                    checked: false
                },
                MenuItemState {
                    id: 200,
                    enabled: true,
                    checked: true
                },
                MenuItemState {
                    id: 300,
                    enabled: true,
                    checked: false
                },
            ],
            "separator nodes contribute no state"
        );
    }

    /// Every leaf — popup children and top-level leaves alike — yields a
    /// state, so a guest flipping any item is picked up by the bar sync.
    #[test]
    fn menu_item_states_covers_every_leaf_once() {
        let nodes = vec![
            MenuNode {
                id: 1,
                title: "Edit".to_owned(),
                enabled: true,
                checked: false,
                separator: false,
                accelerator: None,
                children: vec![leaf(10, "Undo", false, false)],
            },
            MenuNode {
                id: 2,
                title: "View".to_owned(),
                enabled: true,
                checked: false,
                separator: false,
                accelerator: None,
                children: vec![
                    leaf(20, "Status Bar", true, true),
                    MenuNode {
                        id: 3,
                        title: "Zoom".to_owned(),
                        enabled: true,
                        checked: false,
                        separator: false,
                        accelerator: None,
                        children: vec![leaf(30, "Zoom In", true, false)],
                    },
                ],
            },
        ];
        let ids: Vec<u32> = menu_item_states(&nodes).iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![10, 20, 30], "one state per leaf, DFS order");
    }

    /// The id stamped into a built leaf's `MenuId` round-trips back to the
    /// guest id the state list carries — the alignment check
    /// [`apply_item_states`] relies on when matching state to items.
    #[test]
    fn leaf_ids_match_their_menu_item_states() {
        let nodes = vec![leaf(42, "Exit", true, false)];
        let leaves: Vec<LeafItem> = nodes.iter().map(LeafItem::new).collect();
        for (state, leaf) in menu_item_states(&nodes).iter().zip(&leaves) {
            assert_eq!(
                leaf.id().0,
                state.id.to_string(),
                "the bar stamps each item with its guest command id"
            );
        }
    }

    /// The guest's Windows-style shortcut suffixes — the exact set found in
    /// RNotepad's resources — parse into muda accelerators, with "Ctrl"
    /// normalized to the platform's primary command modifier.
    #[test]
    fn parse_menu_accelerator_handles_rnotepad_suffixes() {
        use muda::accelerator::Modifiers;

        let cases = [
            "Ctrl+N",
            "Ctrl+O",
            "Ctrl+Shift+N",
            "F3",
            "F5",
            "Del",
            "Strg+Umschalt+N", // German
            "Entf",            // German delete
            "Sil",             // Turkish delete
            "Ctrl+Shift+S",
        ];
        for suffix in cases {
            let parsed = parse_menu_accelerator(suffix);
            assert!(
                parsed.is_some(),
                "suffix {suffix:?} must parse to an accelerator"
            );
        }

        // Ctrl/Strg map to Command on macOS (muda's CMD_OR_CTRL), matching
        // how modern macOS apps present the primary shortcut.
        let ctrl_n = parse_menu_accelerator("Ctrl+N").expect("Ctrl+N");
        let strg_n = parse_menu_accelerator("Strg+N").expect("Strg+N");
        assert_eq!(ctrl_n, strg_n, "Ctrl and Strg are the same modifier");
        assert!(
            ctrl_n.modifiers().contains(Modifiers::SUPER),
            "Ctrl → Command"
        );
        // F5 keeps its bare function key.
        let f5 = parse_menu_accelerator("F5").expect("F5");
        assert!(f5.modifiers().is_empty(), "bare F5 has no modifiers");
        assert_eq!(
            f5,
            "F5".parse::<Accelerator>().expect("muda parses F5"),
            "F5 passes through to muda unchanged"
        );

        // Garbage falls back to None — the item renders without a shortcut.
        assert_eq!(parse_menu_accelerator("Ctrl+MouseClick"), None);
        assert_eq!(parse_menu_accelerator(""), None);
    }

    /// A separator node contributes no state, so the state list stays
    /// aligned with the retained leaves when a menu groups its items with
    /// separators — `append_children` skips separator nodes with the same
    /// walk, so both sides of the zip line up by construction.
    #[test]
    fn separators_do_not_shift_state_alignment() {
        let nodes = vec![MenuNode {
            id: 1,
            title: "Edit".to_owned(),
            enabled: true,
            checked: false,
            separator: false,
            accelerator: None,
            children: vec![
                leaf(10, "Cut", true, false),
                separator(),
                leaf(11, "Find", true, false),
                separator(),
                leaf(12, "Select All", true, false),
            ],
        }];
        let states = menu_item_states(&nodes);
        assert_eq!(
            states.iter().map(|s| s.id).collect::<Vec<u32>>(),
            vec![10, 11, 12],
            "separators are transparent to the state walk"
        );
    }
}
