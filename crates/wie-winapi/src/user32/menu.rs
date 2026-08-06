use super::{
    Context, Result, WinApiHandlerResult, WinApiState, WindowClassRecord, allocate_menu_handle,
    read_guest_ansi_lossy, read_guest_utf16_lossy, read_typed_copy, write_guest_ansi_c_string,
    write_guest_utf16_c_string, write_typed_copy,
};
use crate::HandlerContext;
use crate::guest_layout::MenuItemInfo;
use crate::handles::{Hmenu, Hwnd};
use wie_pe::resources::{MenuItemTemplate, MenuTemplate};

/// One fake USER32 menu: its handle and the ordered item list. A `Popup`
/// entry references another menu by handle, so the flat `Vec<MenuRecord>`
/// actually forms a tree.
#[derive(Debug, Clone)]
pub struct MenuRecord {
    /// Fake menu handle.
    pub handle: Hmenu,
    /// Items in append order; a `Vec` index is the `MF_BYPOSITION` position.
    pub items: Vec<MenuEntry>,
}

/// One item of a fake menu.
#[derive(Debug, Clone)]
pub enum MenuEntry {
    /// `MF_STRING` command item.
    Item {
        /// Command id (`wID`).
        id: u32,
        /// Item text.
        text: String,
        /// Whether the item is enabled (`MF_GRAYED`/`MF_DISABLED` clear it).
        enabled: bool,
        /// Whether the item is checked (`MF_CHECKED`).
        checked: bool,
    },
    /// `MF_POPUP` item — a structural link to another menu.
    Popup {
        /// Popup label text.
        text: String,
        /// Handle of the submenu this entry opens.
        submenu: Hmenu,
    },
    /// `MF_SEPARATOR`.
    Separator,
}

/// `MF_*` flags relevant to the mechanical menu tier. `MF_POPUP` and
/// `MF_SEPARATOR` are `pub(crate)` so the lib tests name the flags instead of
/// hardcoding the values.
pub(crate) const MF_SEPARATOR: u32 = 0x0800;
const MF_BYPOSITION: u32 = 0x0400;
pub(crate) const MF_POPUP: u32 = 0x0010;
const MF_CHECKED: u32 = 0x0008;
const MF_DISABLED: u32 = 0x0002;
const MF_GRAYED: u32 = 0x0001;

/// `MENUITEMINFO` `fMask` bits.
const MIIM_STATE: u32 = 0x0001;
const MIIM_ID: u32 = 0x0002;
const MIIM_TYPE: u32 = 0x0010;

/// `MENUITEMINFO` `fType` for a string item.
const MFT_STRING: u32 = 0x0000;

/// Handles `USER32.dll!EnableMenuItem`.
pub fn handle_enable_menu_item(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .context("failed to read RCX for EnableMenuItem")?,
    );

    let item_raw = engine
        .read_rdx()
        .context("failed to read RDX for EnableMenuItem")?;

    let flags_raw = engine
        .read_r8()
        .context("failed to read R8 for EnableMenuItem")?;

    let item = u32::try_from(item_raw & u64::from(u32::MAX))
        .context("EnableMenuItem item does not fit u32")?;

    let flags = u32::try_from(flags_raw & u64::from(u32::MAX))
        .context("EnableMenuItem flags do not fit u32")?;

    let new_enabled = flags & (MF_GRAYED | MF_DISABLED) == 0;
    let (previous_flags, mutated) = mutate_item(
        state,
        menu_handle,
        item,
        flags & MF_BYPOSITION != 0,
        |entry| {
            if let MenuEntry::Item { enabled, .. } = entry {
                *enabled = new_enabled;
                true
            } else {
                false
            }
        },
    );

    if mutated {
        state.window_state().menu_dirty = true;
    }

    // Windows returns -1 when no item matches; otherwise the previous state.
    let return_value = u64::from(if mutated { previous_flags } else { u32::MAX });

    ctx.finish(return_value)
}
/// Handles `USER32.dll!CheckMenuItem`.
pub fn handle_check_menu_item(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .context("failed to read RCX for CheckMenuItem")?,
    );

    let item_raw = engine
        .read_rdx()
        .context("failed to read RDX for CheckMenuItem")?;

    let flags_raw = engine
        .read_r8()
        .context("failed to read R8 for CheckMenuItem")?;

    let item = u32::try_from(item_raw & u64::from(u32::MAX))
        .context("CheckMenuItem item does not fit u32")?;

    let flags = u32::try_from(flags_raw & u64::from(u32::MAX))
        .context("CheckMenuItem flags do not fit u32")?;

    let new_checked = flags & MF_CHECKED != 0;
    let (previous_flags, mutated) = mutate_item(
        state,
        menu_handle,
        item,
        flags & MF_BYPOSITION != 0,
        |entry| {
            if let MenuEntry::Item { checked, .. } = entry {
                *checked = new_checked;
                true
            } else {
                false
            }
        },
    );

    if mutated {
        state.window_state().menu_dirty = true;
    }

    let return_value = u64::from(if mutated { previous_flags } else { u32::MAX });

    ctx.finish(return_value)
}
pub(crate) fn handle_menu_success(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}
/// Handles `USER32.dll!GetMenu` — returns the HMENU for a window, or 0.
pub fn handle_get_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = Hwnd::from(engine.read_rcx()?);
    let menu_handle = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == hwnd)
        .map_or(0, |w| w.menu_handle);
    ctx.finish(menu_handle)
}
/// Handles `USER32.dll!CreateMenu`.
pub fn handle_create_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    state.window_state().menus.push(MenuRecord {
        handle: Hmenu::from(handle),
        items: Vec::new(),
    });
    ctx.finish(handle)
}
/// Handles `USER32.dll!CreatePopupMenu`.
pub fn handle_create_popup_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    state.window_state().menus.push(MenuRecord {
        handle: Hmenu::from(handle),
        items: Vec::new(),
    });
    ctx.finish(handle)
}
/// One loaded fake resource menu (`LoadMenuA/W` or a class `lpszMenuName`).
///
/// The parsed items live on `ProcessState::main_module_menus`, so the record
/// only carries the (module, resource id) pair needed to resolve them — and
/// that pair doubles as the cache key, so repeated loads return the same
/// `HMENU` (mirrors `AccelRecord`).
#[derive(Debug, Clone)]
pub struct ResourceMenuRecord {
    /// Fake `HMENU` returned to the guest.
    pub handle: Hmenu,
    /// Module instance the menu was loaded from.
    pub instance_handle: u64,
    /// Resource id the menu was loaded under.
    pub menu_id: u16,
}

/// Handles `USER32.dll!LoadMenuW`.
pub fn handle_load_menu_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_menu_impl(ctx, "LoadMenuW")
}
/// Handles `USER32.dll!LoadMenuA`.
pub fn handle_load_menu_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_menu_impl(ctx, "LoadMenuA")
}

/// Shared `LoadMenuA/W` implementation.
///
/// Win64 ABI: `rcx` = hinst, `rdx` = `lpMenuName`. Only `MAKEINTRESOURCE`
/// ids are resolvable — a string-named menu has no parsed name→template
/// mapping and returns `NULL`, mirroring how `LoadStringA/W` and
/// `LoadAcceleratorsA/W` resolve ids only. The resource is only parsed for
/// the main EXE module, so any other `hinst` also resolves to `NULL`.
fn handle_load_menu_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let menu_name_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let menu_id = if menu_name_raw >> 16 == 0 {
        u16::try_from(menu_name_raw & u64::from(u16::MAX)).unwrap_or(0)
    } else {
        0
    };

    let return_value = if menu_id == 0 || instance_handle != ctx.environment.image_base {
        // Not the main EXE (or a named menu): no parsed template to resolve.
        0
    } else {
        load_resource_menu(state, instance_handle, menu_id)?
    };

    tracing::debug!(
        target: "wiegui",
        instance_handle,
        menu_id,
        hmenu = return_value,
        "{api_name}"
    );

    ctx.finish(return_value)
}

/// Resolve `(instance_handle, menu_id)` to a fake `HMENU`, caching it.
///
/// The template is converted into the same flat `MenuRecord` tree the
/// programmatic `CreateMenu`/`AppendMenu` path builds, so `GetMenuState`,
/// `GetMenuItemInfo` and the host `MacMenuBar` mirror work unchanged. An id
/// that does not match any parsed template resolves to `NULL`; a repeat load
/// of the same pair returns the previously allocated handle.
pub(crate) fn load_resource_menu(
    state: &mut WinApiState,
    instance_handle: u64,
    menu_id: u16,
) -> Result<u64> {
    // When the id exists in several locales, the UI language picks the
    // template (exact LANGID → neutral → en-US → first in directory order).
    let ui_language = super::lang::ui_language();
    let template = super::lang::resolve_block(
        state
            .process
            .main_module_menus
            .iter()
            .filter(|template| template.id == u32::from(menu_id))
            .map(|template| (u32::from(template.lang), template)),
        ui_language,
    )
    .cloned();
    let Some(template) = template else {
        return Ok(0);
    };

    if let Some(record) = state
        .window_state()
        .resource_menus
        .iter()
        .find(|record| record.instance_handle == instance_handle && record.menu_id == menu_id)
    {
        return Ok(record.handle.as_u64());
    }

    let handle = allocate_menu_handle(state)?;
    build_template_menu_records(state, &template, handle)?;
    state
        .window_state()
        .resource_menus
        .push(ResourceMenuRecord {
            handle: Hmenu::from(handle),
            instance_handle,
            menu_id,
        });
    state.window_state().menu_dirty = true;
    Ok(handle)
}

/// Build the `MenuRecord` tree for a parsed `MenuTemplate`.
///
/// The top-level record gets `handle`; `MF_POPUP` items get fresh child
/// records (allocated before their parent entry references them), and
/// `MF_STRING`/`MF_SEPARATOR` items map 1:1 onto `MenuEntry`.
fn build_template_menu_records(
    state: &mut WinApiState,
    template: &MenuTemplate,
    handle: u64,
) -> Result<()> {
    let mut items = Vec::new();
    for item in &template.items {
        items.push(build_template_menu_entry(state, item)?);
    }
    state.window_state().menus.push(MenuRecord {
        handle: Hmenu::from(handle),
        items,
    });
    Ok(())
}

/// Convert one parsed `MenuItemTemplate` into the native `MenuEntry` the
/// programmatic path uses, building child records for popup submenus.
///
/// A separator is recognized two ways: the explicit `MF_SEPARATOR` bit, or
/// `text: None` — `windres` emits a `MENUITEM SEPARATOR` as a zero option
/// word + empty string, which the parser keeps as `flags: 0, text: None`.
fn build_template_menu_entry(
    state: &mut WinApiState,
    item: &MenuItemTemplate,
) -> Result<MenuEntry> {
    if item.flags & MF_POPUP != 0 {
        let submenu_handle = allocate_menu_handle(state)?;
        let mut sub_items = Vec::new();
        for sub in &item.sub {
            sub_items.push(build_template_menu_entry(state, sub)?);
        }
        state.window_state().menus.push(MenuRecord {
            handle: Hmenu::from(submenu_handle),
            items: sub_items,
        });
        Ok(MenuEntry::Popup {
            text: item.text.clone().unwrap_or_default(),
            submenu: Hmenu::from(submenu_handle),
        })
    } else if item.flags & MF_SEPARATOR != 0 || item.text.is_none() {
        Ok(MenuEntry::Separator)
    } else {
        Ok(MenuEntry::Item {
            id: item.id,
            text: item.text.clone().unwrap_or_default(),
            enabled: item.flags & (MF_GRAYED | MF_DISABLED) == 0,
            checked: item.flags & MF_CHECKED != 0,
        })
    }
}

/// Resolve the `HMENU` a new window record carries.
///
/// An explicit `hMenu` argument wins; otherwise a registered class's
/// `lpszMenuName` — a `MAKEINTRESOURCE` menu resource of the class's
/// module — is loaded (Windows applies the class menu to top-level windows
/// created without a menu). String-named class menus and unknown ids resolve
/// to no menu.
pub(crate) fn resolve_class_menu(
    state: &mut WinApiState,
    explicit_menu: u64,
    class: Option<&WindowClassRecord>,
) -> Result<u64> {
    if explicit_menu != 0 {
        return Ok(explicit_menu);
    }
    let Some(class) = class else {
        return Ok(0);
    };
    if class.instance_handle == 0 || class.menu_name == 0 {
        return Ok(0);
    }
    if class.menu_name >> 16 != 0 {
        return Ok(0); // string-named class menu: no parsed name→template mapping
    }
    let menu_id = u16::try_from(class.menu_name & u64::from(u16::MAX)).unwrap_or(0);
    if menu_id == 0 {
        return Ok(0);
    }
    load_resource_menu(state, class.instance_handle, menu_id)
}
/// Handles `USER32.dll!AppendMenuA`.
pub fn handle_append_menu_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_append_menu_impl(ctx, "AppendMenuA", false)
}
/// Handles `USER32.dll!AppendMenuW`.
pub fn handle_append_menu_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_append_menu_impl(ctx, "AppendMenuW", true)
}
fn handle_append_menu_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .with_context(|| format!("failed to read RCX for {api_name}"))?,
    );

    let flags_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let item_id_raw = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let item_text_ptr = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    let flags = u32::try_from(flags_raw & u64::from(u32::MAX))
        .with_context(|| format!("{api_name} flags do not fit u32"))?;

    let item_id = u32::try_from(item_id_raw & u64::from(u32::MAX))
        .with_context(|| format!("{api_name} item id does not fit u32"))?;

    let text = if item_text_ptr == 0 || flags & MF_SEPARATOR != 0 {
        String::new()
    } else if unicode {
        read_guest_utf16_lossy(engine, item_text_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} item text"))?
    } else {
        read_guest_ansi_lossy(engine, item_text_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} item text"))?
    };

    let ws = state.window_state();
    if !ws.menus.iter().any(|m| m.handle == menu_handle) {
        // Lazily create the record for handles that bypassed CreateMenu /
        // CreatePopupMenu (e.g. GetSystemMenu).
        ws.menus.push(MenuRecord {
            handle: menu_handle,
            items: Vec::new(),
        });
    }
    let entry = if flags & MF_SEPARATOR != 0 {
        MenuEntry::Separator
    } else if flags & MF_POPUP != 0 {
        MenuEntry::Popup {
            text,
            submenu: Hmenu::from(u64::from(item_id)),
        }
    } else {
        MenuEntry::Item {
            id: item_id,
            text,
            enabled: flags & (MF_GRAYED | MF_DISABLED) == 0,
            checked: flags & MF_CHECKED != 0,
        }
    };
    let record = ws
        .menus
        .iter_mut()
        .find(|m| m.handle == menu_handle)
        .with_context(|| {
            format!(
                "{api_name}: menu {:#x} record missing",
                menu_handle.as_u64()
            )
        })?;
    record.items.push(entry);
    ws.menu_dirty = true;

    ctx.finish(1)
}
/// Handles `USER32.dll!SetMenu` — stores the handle on the window record.
///
/// The host reads `WindowRecord.menu_handle` (via `GetMenu`) to surface the
/// window's menu in the macOS application menu bar, so a success stub that
/// drops the handle would leave that path empty.
pub fn handle_set_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = Hwnd::from(
        engine
            .read_rcx()
            .context("failed to read RCX for SetMenu")?,
    );
    let menu_handle = engine
        .read_rdx()
        .context("failed to read RDX for SetMenu")?;
    let ws = state.window_state();
    if let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) {
        window.menu_handle = menu_handle;
        ws.menu_dirty = true;
    }
    ctx.finish(1)
}
/// Handles `USER32.dll!DestroyMenu`.
pub fn handle_destroy_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .context("failed to read RCX for DestroyMenu")?,
    );
    let ws = state.window_state();
    // YAGNI: DestroyMenu does not tear down resource_menus entries — a
    // resource menu is a (module, id)-keyed cache shared by repeated
    // LoadMenu calls, not a per-destroy handle.
    let before = ws.menus.len();
    ws.menus.retain(|m| m.handle != menu_handle);
    if ws.menus.len() != before {
        ws.menu_dirty = true;
    }
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!RemoveMenu`.
pub fn handle_remove_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!DeleteMenu`.
pub fn handle_delete_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!ModifyMenuA`.
pub fn handle_modify_menu_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!ModifyMenuW`.
pub fn handle_modify_menu_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!GetSystemMenu`.
pub fn handle_get_system_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    ctx.finish(handle)
}
/// Handles `USER32.dll!TrackPopupMenu`.
pub fn handle_track_popup_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    // No item selected.
    ctx.finish(0)
}
/// Handles `USER32.dll!GetMenuItemInfoA`.
pub fn handle_get_menu_item_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_menu_item_info_impl(ctx, "GetMenuItemInfoA", false)
}
/// Handles `USER32.dll!GetMenuItemInfoW`.
pub fn handle_get_menu_item_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_menu_item_info_impl(ctx, "GetMenuItemInfoW", true)
}
fn handle_get_menu_item_info_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .with_context(|| format!("failed to read RCX for {api_name}"))?,
    );

    let item_value = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let by_position_raw = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let info_ptr = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    let by_position = by_position_raw != 0;
    let success = info_ptr != 0
        && fill_menu_item_info(
            engine,
            state,
            menu_handle,
            item_value,
            by_position,
            info_ptr,
            unicode,
        )
        .with_context(|| format!("failed to fill {api_name} MENUITEMINFO"))?;

    let return_value = u64::from(success);

    ctx.finish(return_value)
}

/// Reconstructed `MF_*` state flags for an entry, as reported by
/// `GetMenuState` and `MENUITEMINFO.fState`.
fn entry_flags(entry: &MenuEntry) -> u32 {
    match entry {
        MenuEntry::Item {
            enabled, checked, ..
        } => {
            let mut flags = 0;
            if !*enabled {
                flags |= MF_GRAYED;
            }
            if *checked {
                flags |= MF_CHECKED;
            }
            flags
        }
        MenuEntry::Popup { .. } => MF_POPUP,
        MenuEntry::Separator => MF_SEPARATOR,
    }
}

/// `wID` a menu entry reports; popups report their submenu handle, matching
/// the `(UINT_PTR)` id AppendMenu stored.
fn entry_id(entry: &MenuEntry) -> u32 {
    match entry {
        MenuEntry::Item { id, .. } => *id,
        MenuEntry::Popup { submenu, .. } => u32::try_from(submenu.as_u64()).unwrap_or(u32::MAX),
        MenuEntry::Separator => 0,
    }
}

/// Position of an entry within the menus tree.
struct EntryPos {
    record: usize,
    item: usize,
}

/// Depth-first search for the first entry with command id `item_id`, starting
/// at the menu `menu_handle` and descending into popup submenus.
fn find_entry_pos(menus: &[MenuRecord], menu_handle: Hmenu, item_id: u32) -> Option<EntryPos> {
    let record_index = menus.iter().position(|m| m.handle == menu_handle)?;
    let mut visited = vec![record_index];
    find_in_record(menus, record_index, item_id, &mut visited)
}

fn find_in_record(
    menus: &[MenuRecord],
    record_index: usize,
    item_id: u32,
    visited: &mut Vec<usize>,
) -> Option<EntryPos> {
    let record = menus.get(record_index)?;
    for (index, entry) in record.items.iter().enumerate() {
        match entry {
            MenuEntry::Item { id, .. } if *id == item_id => {
                return Some(EntryPos {
                    record: record_index,
                    item: index,
                });
            }
            MenuEntry::Popup { submenu, .. } => {
                let Some(sub_index) = menus.iter().position(|m| m.handle == *submenu) else {
                    continue;
                };
                if visited.contains(&sub_index) {
                    continue; // cycle guard
                }
                visited.push(sub_index);
                if let Some(pos) = find_in_record(menus, sub_index, item_id, visited) {
                    return Some(pos);
                }
                visited.pop();
            }
            _ => {}
        }
    }
    None
}

/// Resolve `(menu_handle, item_value, by_position)` to an entry position:
/// by position the value is a `Vec` index of that menu's items; otherwise a
/// depth-first command-id search.
fn resolve_entry_pos(
    menus: &[MenuRecord],
    menu_handle: Hmenu,
    item_value: u64,
    by_position: bool,
) -> Option<EntryPos> {
    let record_index = menus.iter().position(|m| m.handle == menu_handle)?;
    if by_position {
        let position = usize::try_from(item_value).ok()?;
        menus.get(record_index)?.items.get(position)?;
        Some(EntryPos {
            record: record_index,
            item: position,
        })
    } else {
        let item_id = u32::try_from(item_value & u64::from(u32::MAX)).ok()?;
        find_entry_pos(menus, menu_handle, item_id)
    }
}

/// Apply `mutate` to the item found by `(menu_handle, item_value, by_position)`.
/// Returns the previous reconstructed flags and whether the item was mutated.
fn mutate_item(
    state: &mut WinApiState,
    menu_handle: Hmenu,
    item_value: u32,
    by_position: bool,
    mutate: impl FnOnce(&mut MenuEntry) -> bool,
) -> (u32, bool) {
    let ws = state.window_state();
    let Some(pos) = resolve_entry_pos(&ws.menus, menu_handle, u64::from(item_value), by_position)
    else {
        return (u32::MAX, false);
    };
    let Some(entry) = ws
        .menus
        .get_mut(pos.record)
        .and_then(|m| m.items.get_mut(pos.item))
    else {
        return (u32::MAX, false);
    };
    let previous = entry_flags(entry);
    (previous, mutate(entry))
}

/// Data a guest-visible menu query needs (`GetMenuState`, `GetMenuItemInfo`).
struct ResolvedMenuEntry {
    /// `wID` reported to the guest (submenu handle for popups).
    id: u32,
    /// Item text (empty for separators).
    text: String,
    /// Reconstructed `MF_*` state flags.
    flags: u32,
}

/// Look up a menu entry by id (recursive) or by position.
fn find_menu_entry(
    menus: &[MenuRecord],
    menu_handle: Hmenu,
    item_value: u64,
    by_position: bool,
) -> Option<ResolvedMenuEntry> {
    let record_index = menus.iter().position(|m| m.handle == menu_handle)?;
    let entry = if by_position {
        let position = usize::try_from(item_value).ok()?;
        menus.get(record_index)?.items.get(position)?
    } else {
        let item_id = u32::try_from(item_value & u64::from(u32::MAX)).ok()?;
        let pos = find_entry_pos(menus, menu_handle, item_id)?;
        menus.get(pos.record)?.items.get(pos.item)?
    };
    Some(ResolvedMenuEntry {
        id: entry_id(entry),
        text: match entry {
            MenuEntry::Item { text, .. } | MenuEntry::Popup { text, .. } => text.clone(),
            MenuEntry::Separator => String::new(),
        },
        flags: entry_flags(entry),
    })
}

/// Fill the guest `MENUITEMINFO` for the requested `fMask` bits.
fn fill_menu_item_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    menu_handle: Hmenu,
    item_value: u64,
    by_position: bool,
    info_ptr: u64,
    unicode: bool,
) -> Result<bool> {
    let Some(item) = find_menu_entry(
        &state.window_state().menus,
        menu_handle,
        item_value,
        by_position,
    ) else {
        return Ok(false);
    };

    // MENUITEMINFO is an in/out struct: the guest's untouched fields (cbSize,
    // hSubMenu, hbmp*, dwItemData, hbmpItem) must survive the write. Snapshot
    // the whole struct (it is Copy), edit the mask-selected fields, write it
    // back — two shared-lock borrows with engine I/O (the string writes)
    // between them. The layout + pinned offsets (dwTypeData @56, cch @64)
    // live in `crate::guest_layout::MenuItemInfo`.
    let mut info =
        read_typed_copy::<MenuItemInfo>(engine, info_ptr).context("failed to read MENUITEMINFO")?;

    if info.f_mask & MIIM_STATE != 0 {
        info.f_state = item.flags & 0x00ff;
    }

    if info.f_mask & MIIM_ID != 0 {
        info.w_id = item.id;
    }

    if info.f_mask & MIIM_TYPE != 0 {
        info.f_type = MFT_STRING;

        let capacity = usize::try_from(info.cch).context("MENUITEMINFO.cch does not fit usize")?;

        if info.type_data_ptr != 0 && capacity > 0 {
            let copied = if unicode {
                write_guest_utf16_c_string(engine, info.type_data_ptr, capacity, &item.text)?
            } else {
                write_guest_ansi_c_string(engine, info.type_data_ptr, capacity, &item.text)?
            };
            info.cch = u32::try_from(copied).context("menu text length does not fit u32")?;
        }
    }

    write_typed_copy(engine, info_ptr, info).context("failed to write MENUITEMINFO")?;

    Ok(true)
}

/// Handles `USER32.dll!GetMenuState`.
pub fn handle_get_menu_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = Hmenu::from(
        engine
            .read_rcx()
            .context("failed to read RCX for GetMenuState")?,
    );

    let item_value = engine
        .read_rdx()
        .context("failed to read RDX for GetMenuState")?;

    let flags_raw = engine
        .read_r8()
        .context("failed to read R8 for GetMenuState")?;

    let flags = u32::try_from(flags_raw & u64::from(u32::MAX))
        .context("GetMenuState flags do not fit u32")?;

    let by_position = flags & MF_BYPOSITION != 0;

    let return_value = find_menu_entry(
        &state.window_state().menus,
        menu_handle,
        item_value,
        by_position,
    )
    .map_or(u64::from(u32::MAX), |item| u64::from(item.flags));

    ctx.finish(return_value)
}

/// Handles `USER32.dll!DrawMenuBar` (rendering deferred; always succeeds).
pub fn handle_draw_menu_bar(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!SetMenuItemInfoA`.
pub fn handle_set_menu_item_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!SetMenuItemInfoW`.
pub fn handle_set_menu_item_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
/// Handles `USER32.dll!CheckMenuRadioItem`.
pub fn handle_check_menu_radio_item(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx)
}
