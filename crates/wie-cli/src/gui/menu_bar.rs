//! Safe bridge from the guest's window menu to the macOS application menu bar.
//!
//! Built on the `muda` crate, which wraps AppKit behind a safe API — this
//! module contains no raw Objective-C (no `unsafe`, no `declare_class!`, no
//! `msg_send!`). muda objects may only be touched on the main thread (muda
//! panics if a [`Menu`] is created off it), so every method here checks the
//! thread [`MacMenuBar::new`] ran on and no-ops otherwise; the next `Frame`
//! retries.

use std::thread::ThreadId;

use muda::{Menu, MenuEvent, MenuId, MenuItem, Submenu};
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
        for node in items {
            if node.children.is_empty() {
                // Leaf: a top-level command item.
                let item = MenuItem::with_id(guest_id_to_menu_id(node.id), &node.title, true, None);
                if let Err(error) = menu.append(&item) {
                    tracing::warn!(error = %error, "muda: failed to append menu item");
                }
            } else {
                // Popup: a top-level menu holding the children.
                let submenu = Submenu::new(&node.title, true);
                append_children(&submenu, &node.children);
                // AppKit retains the appended items (and the root menu retains
                // the submenu), so the local wrapper may drop after append.
                if let Err(error) = menu.append(&submenu) {
                    tracing::warn!(error = %error, "muda: failed to append submenu");
                }
            }
        }
        menu.init_for_nsapp();
        self.menu = Some(menu);
    }
}

/// Recursively append `nodes` into `submenu` (leaf items and nested popups).
fn append_children(submenu: &Submenu, nodes: &[MenuNode]) {
    for node in nodes {
        if node.children.is_empty() {
            let item = MenuItem::with_id(guest_id_to_menu_id(node.id), &node.title, true, None);
            if let Err(error) = submenu.append(&item) {
                tracing::warn!(error = %error, "muda: failed to append menu item");
            }
        } else {
            let child = Submenu::new(&node.title, true);
            append_children(&child, &node.children);
            if let Err(error) = submenu.append(&child) {
                tracing::warn!(error = %error, "muda: failed to append submenu");
            }
        }
    }
}
