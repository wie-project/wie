use super::*;

pub fn handle_enable_menu_item(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
        .window_state.menu_item_states
        .iter()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
        .map_or(u32::MAX, |(_, _, stored_flags)| *stored_flags);

    if let Some(entry) = state
        .window_state.menu_item_states
        .iter_mut()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
    {
        entry.2 = flags;
    } else {
        state.window_state.menu_item_states.push((menu_handle, item, flags));
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
pub fn handle_check_menu_item(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
        .window_state.menu_item_check_states
        .iter()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
        .map_or(u32::MAX, |(_, _, stored_flags)| *stored_flags);

    if let Some(entry) = state
        .window_state.menu_item_check_states
        .iter_mut()
        .find(|(stored_menu, stored_item, _)| *stored_menu == menu_handle && *stored_item == item)
    {
        entry.2 = flags;
    } else {
        state
            .window_state.menu_item_check_states
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
    engine: &mut dyn wie_cpu::CpuEngine,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(1)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub fn handle_get_menu(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let hwnd = engine.read_rcx()?;
    let menu_handle = state
        .window_state.windows
        .iter()
        .find(|w| w.handle == hwnd)
        .map_or(0, |w| w.menu_handle);
    let return_address = engine.return_from_win64_api(menu_handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: menu_handle,
    })
}
pub fn handle_create_menu(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from CreateMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
pub fn handle_create_popup_menu(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from CreatePopupMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
pub fn handle_append_menu_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "AppendMenuA")
}
pub fn handle_append_menu_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "AppendMenuW")
}
pub fn handle_set_menu(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "SetMenu")
}
pub fn handle_destroy_menu(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "DestroyMenu")
}
pub fn handle_remove_menu(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "RemoveMenu")
}
pub fn handle_delete_menu(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "DeleteMenu")
}
pub fn handle_modify_menu_a(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "ModifyMenuA")
}
pub fn handle_modify_menu_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "ModifyMenuW")
}
pub fn handle_get_system_menu(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = allocate_menu_handle(state)?;
    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from GetSystemMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
pub fn handle_track_popup_menu(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    // No item selected.
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from TrackPopupMenu")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
pub fn handle_get_menu_item_info_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "GetMenuItemInfoA")
}
pub fn handle_get_menu_item_info_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "GetMenuItemInfoW")
}
pub fn handle_set_menu_item_info_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "SetMenuItemInfoA")
}
pub fn handle_set_menu_item_info_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "SetMenuItemInfoW")
}
pub fn handle_check_menu_radio_item(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    handle_menu_success(engine, "CheckMenuRadioItem")
}