use super::{
    Context, Result, WinApiHandlerResult, WinApiState, allocate_menu_handle, checked_field_address,
    read_guest_ansi_lossy, read_guest_u32, read_guest_u64, read_guest_utf16_lossy,
    write_guest_ansi_c_string, write_guest_u32, write_guest_utf16_c_string,
};
use crate::{HandlerContext, MenuItemRecord};

/// `MF_*` flags relevant to the mechanical menu tier.
const MF_SEPARATOR: u32 = 0x0800;
const MF_BYPOSITION: u32 = 0x0400;

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
    let menu_handle = engine
        .read_rcx()
        .context("failed to read RCX for EnableMenuItem")?;

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

    let previous_flags = state
        .window_state()
        .menu_item_states
        .iter()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
        .map_or(u32::MAX, |(_, _, stored_flags)| *stored_flags);

    if let Some(entry) = state
        .window_state()
        .menu_item_states
        .iter_mut()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
    {
        entry.2 = flags;
    } else {
        state
            .window_state()
            .menu_item_states
            .push((menu_handle, item, flags));
    }

    let return_value = u64::from(previous_flags);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EnableMenuItem")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!CheckMenuItem`.
pub fn handle_check_menu_item(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = engine
        .read_rcx()
        .context("failed to read RCX for CheckMenuItem")?;

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

    let previous_flags = state
        .window_state()
        .menu_item_check_states
        .iter()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
        .map_or(u32::MAX, |(_, _, stored_flags)| *stored_flags);

    if let Some(entry) =
        state.window_state().menu_item_check_states.iter_mut().find(
            |(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item,
        )
    {
        entry.2 = flags;
    } else {
        state
            .window_state()
            .menu_item_check_states
            .push((menu_handle, item, flags));
    }

    let return_value = u64::from(previous_flags);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CheckMenuItem")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub(crate) fn handle_menu_success(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!GetMenu` — returns the HMENU for a window, or 0.
pub fn handle_get_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine.read_rcx()?;
    let menu_handle = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == hwnd)
        .map_or(0, |w| w.menu_handle);
    let return_address = engine.return_from_win64_api(menu_handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: menu_handle,
    })
}
/// Handles `USER32.dll!CreateMenu`.
pub fn handle_create_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from CreateMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `USER32.dll!CreatePopupMenu`.
pub fn handle_create_popup_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from CreatePopupMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `USER32.dll!AppendMenuA`.
pub fn handle_append_menu_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_append_menu(ctx, "AppendMenuA", false)
}
/// Handles `USER32.dll!AppendMenuW`.
pub fn handle_append_menu_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_append_menu(ctx, "AppendMenuW", true)
}
fn handle_append_menu(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

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

    state.window_state().menu_items.push(MenuItemRecord {
        menu_handle,
        id: item_id,
        flags,
        text,
    });

    let return_address = engine
        .return_from_win64_api(1)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!SetMenu` — stores the handle on the window record.
///
/// The host reads `WindowRecord.menu_handle` (via `GetMenu`) to surface the
/// window's menu in the macOS application menu bar, so a success stub that
/// drops the handle would leave that path empty.
pub fn handle_set_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for SetMenu")?;
    let menu_handle = engine
        .read_rdx()
        .context("failed to read RDX for SetMenu")?;
    if let Some(window) = state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == hwnd)
    {
        window.menu_handle = menu_handle;
    }
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from SetMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!DestroyMenu`.
pub fn handle_destroy_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "DestroyMenu")
}
/// Handles `USER32.dll!RemoveMenu`.
pub fn handle_remove_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "RemoveMenu")
}
/// Handles `USER32.dll!DeleteMenu`.
pub fn handle_delete_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "DeleteMenu")
}
/// Handles `USER32.dll!ModifyMenuA`.
pub fn handle_modify_menu_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "ModifyMenuA")
}
/// Handles `USER32.dll!ModifyMenuW`.
pub fn handle_modify_menu_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "ModifyMenuW")
}
/// Handles `USER32.dll!GetSystemMenu`.
pub fn handle_get_system_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from GetSystemMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `USER32.dll!TrackPopupMenu`.
pub fn handle_track_popup_menu(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // No item selected.
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from TrackPopupMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `USER32.dll!GetMenuItemInfoA`.
pub fn handle_get_menu_item_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_menu_item_info(ctx, "GetMenuItemInfoA", false)
}
/// Handles `USER32.dll!GetMenuItemInfoW`.
pub fn handle_get_menu_item_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_menu_item_info(ctx, "GetMenuItemInfoW", true)
}
fn handle_get_menu_item_info(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

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

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Locate a menu item by id (or position when `by_position`).
fn find_menu_item(
    state: &mut WinApiState,
    menu_handle: u64,
    item_value: u64,
    by_position: bool,
) -> Option<MenuItemRecord> {
    let items = &state.window_state().menu_items;
    let mut menu_items = items.iter().filter(|item| item.menu_handle == menu_handle);
    if by_position {
        let position = usize::try_from(item_value).unwrap_or(usize::MAX);
        menu_items.nth(position).cloned()
    } else {
        let item_id = u32::try_from(item_value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
        menu_items.find(|item| item.id == item_id).cloned()
    }
}

/// Fill the guest `MENUITEMINFO` for the requested `fMask` bits.
fn fill_menu_item_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    menu_handle: u64,
    item_value: u64,
    by_position: bool,
    info_ptr: u64,
    unicode: bool,
) -> Result<bool> {
    let Some(item) = find_menu_item(state, menu_handle, item_value, by_position) else {
        return Ok(false);
    };

    // MENUITEMINFO on Win64:
    // cbSize 0, fMask 4, fType 8, fState 12, wID 16, hSubMenu 24,
    // hbmpChecked 32, hbmpUnchecked 40, dwItemData 48, dwTypeData 56,
    // cch 64, hbmpItem 72.
    let f_mask = read_guest_u32(
        engine,
        checked_field_address(info_ptr, 4, "MENUITEMINFO.fMask"),
    )
    .context("failed to read MENUITEMINFO.fMask")?;

    if f_mask & MIIM_STATE != 0 {
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 12, "MENUITEMINFO.fState"),
            item.flags & 0x00ff,
        )
        .context("failed to write MENUITEMINFO.fState")?;
    }

    if f_mask & MIIM_ID != 0 {
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 16, "MENUITEMINFO.wID"),
            item.id,
        )
        .context("failed to write MENUITEMINFO.wID")?;
    }

    if f_mask & MIIM_TYPE != 0 {
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 8, "MENUITEMINFO.fType"),
            MFT_STRING,
        )
        .context("failed to write MENUITEMINFO.fType")?;

        let type_data_ptr = read_guest_u64(
            engine,
            checked_field_address(info_ptr, 56, "MENUITEMINFO.dwTypeData"),
        )
        .context("failed to read MENUITEMINFO.dwTypeData")?;

        let capacity_raw = read_guest_u32(
            engine,
            checked_field_address(info_ptr, 64, "MENUITEMINFO.cch"),
        )
        .context("failed to read MENUITEMINFO.cch")?;

        let capacity =
            usize::try_from(capacity_raw).context("MENUITEMINFO.cch does not fit usize")?;

        if type_data_ptr != 0 && capacity > 0 {
            let copied = if unicode {
                write_guest_utf16_c_string(engine, type_data_ptr, capacity, &item.text)?
            } else {
                write_guest_ansi_c_string(engine, type_data_ptr, capacity, &item.text)?
            };
            let copied = u32::try_from(copied).context("menu text length does not fit u32")?;
            write_guest_u32(
                engine,
                checked_field_address(info_ptr, 64, "MENUITEMINFO.cch"),
                copied,
            )
            .context("failed to write MENUITEMINFO.cch")?;
        }
    }

    Ok(true)
}

/// Handles `USER32.dll!GetMenuState`.
pub fn handle_get_menu_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let menu_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetMenuState")?;

    let item_value = engine
        .read_rdx()
        .context("failed to read RDX for GetMenuState")?;

    let flags_raw = engine
        .read_r8()
        .context("failed to read R8 for GetMenuState")?;

    let flags = u32::try_from(flags_raw & u64::from(u32::MAX))
        .context("GetMenuState flags do not fit u32")?;

    let by_position = flags & MF_BYPOSITION != 0;

    let return_value = find_menu_item(state, menu_handle, item_value, by_position)
        .map_or(u64::from(u32::MAX), |item| u64::from(item.flags));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetMenuState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!DrawMenuBar` (rendering deferred; always succeeds).
pub fn handle_draw_menu_bar(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "DrawMenuBar")
}
/// Handles `USER32.dll!SetMenuItemInfoA`.
pub fn handle_set_menu_item_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "SetMenuItemInfoA")
}
/// Handles `USER32.dll!SetMenuItemInfoW`.
pub fn handle_set_menu_item_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "SetMenuItemInfoW")
}
/// Handles `USER32.dll!CheckMenuRadioItem`.
pub fn handle_check_menu_radio_item(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_menu_success(ctx, "CheckMenuRadioItem")
}
