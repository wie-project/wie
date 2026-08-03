use anyhow::{Context, Result};

use crate::guest_memory::{checked_field_address, write_u32 as write_guest_u32};
use crate::user32::{
    CreateWindowRequest, WS_CHILD, WindowClassIdentifier, create_window_record,
    read_guest_ansi_lossy, read_guest_utf16_lossy,
};
use crate::{HandlerContext, WinApiHandlerResult};

const S_OK: u64 = 0;
const FAKE_IMAGE_LIST_HANDLE: u64 = 0x0000_0000_6900_0001;
const CLR_NONE: u32 = 0xffff_ffff;

/// `STATUSCLASSNAMEW` — the comctl32 status-bar window class name.
///
/// `CreateStatusWindowA/W` always create the child with this class, and it is
/// the name a guest would pass to `CreateWindowEx` for a status bar. The
/// built-in class registry (`ControlClassKind::StatusBar`) recognizes it so
/// `create_window_record` gives the window a host-side control proc.
const STATUSCLASSNAME: &str = "msctls_statusbar32";

/// Handles dynamic `COMCTL32.dll!DllGetVersion`.
pub fn handle_dll_get_version(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let version_info_ptr = engine
        .read_rcx()
        .context("failed to read RCX for DllGetVersion")?;

    if version_info_ptr != 0 {
        // DLLVERSIONINFO:
        // DWORD cbSize;          offset 0
        // DWORD dwMajorVersion;  offset 4
        // DWORD dwMinorVersion;  offset 8
        // DWORD dwBuildNumber;   offset 12
        // DWORD dwPlatformID;    offset 16
        //
        // Common Controls v6-ish fake version.
        write_guest_u32(engine, version_info_ptr, 20)?;
        write_guest_u32(
            engine,
            checked_field_address(version_info_ptr, 4, "dwMajorVersion"),
            6,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(version_info_ptr, 8, "dwMinorVersion"),
            0,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(version_info_ptr, 12, "dwBuildNumber"),
            7600,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(version_info_ptr, 16, "dwPlatformID"),
            1,
        )?;
    }

    let return_address = engine
        .return_from_win64_api(S_OK)
        .context("failed to return from DllGetVersion")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: S_OK,
    })
}

/// Handles `COMCTL32.dll!InitCommonControls` imported as ordinal 17.
pub fn handle_init_common_controls(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from InitCommonControls")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles dynamic `COMCTL32.dll!InitCommonControlsEx`.
pub fn handle_init_common_controls_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let init_common_controls_ex_ptr = engine
        .read_rcx()
        .context("failed to read RCX for InitCommonControlsEx")?;

    let return_value = u64::from(init_common_controls_ex_ptr != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from InitCommonControlsEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `COMCTL32.dll!ImageList_Create`.
pub fn handle_image_list_create(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _icon_width = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_Create")?;

    let _icon_height = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_Create")?;

    let _flags = engine
        .read_r8()
        .context("failed to read R8 for ImageList_Create")?;

    let _initial_count = engine
        .read_r9()
        .context("failed to read R9 for ImageList_Create")?;

    if let Some((_, count)) = state
        .window_state()
        .image_list_counts
        .iter_mut()
        .find(|(handle, _)| *handle == FAKE_IMAGE_LIST_HANDLE)
    {
        *count = 0;
    } else {
        state
            .window_state()
            .image_list_counts
            .push((FAKE_IMAGE_LIST_HANDLE, 0));
    }

    let return_address = engine
        .return_from_win64_api(FAKE_IMAGE_LIST_HANDLE)
        .context("failed to return from ImageList_Create")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_IMAGE_LIST_HANDLE,
    })
}

/// Handles `COMCTL32.dll!ImageList_AddMasked`.
pub fn handle_image_list_add_masked(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_AddMasked")?;

    let bitmap_handle = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_AddMasked")?;

    let _mask_color = engine
        .read_r8()
        .context("failed to read R8 for ImageList_AddMasked")?;

    let return_value = if image_list_handle == FAKE_IMAGE_LIST_HANDLE && bitmap_handle != 0 {
        let count = state
            .window_state()
            .image_list_counts
            .iter_mut()
            .find(|(handle, _)| *handle == image_list_handle)
            .map(|(_, count)| count)
            .context("ImageList_AddMasked received an unregistered image list")?;

        let image_index = *count;

        *count = count
            .checked_add(1)
            .context("image list item count overflow")?;

        image_index
    } else {
        u64::MAX
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ImageList_AddMasked")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `COMCTL32.dll!ImageList_SetBkColor`.
pub fn handle_image_list_set_bk_color(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_SetBkColor")?;

    let background_color_raw = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_SetBkColor")?;

    let background_color = u32::try_from(background_color_raw)
        .context("ImageList_SetBkColor color does not fit u32")?;

    let image_list_exists = state
        .window_state()
        .image_list_counts
        .iter()
        .any(|(handle, _)| *handle == image_list_handle);

    let return_value = if image_list_exists {
        if let Some((_, stored_color)) = state
            .window_state()
            .image_list_background_colors
            .iter_mut()
            .find(|(handle, _)| *handle == image_list_handle)
        {
            let previous_color = *stored_color;
            *stored_color = background_color;
            u64::from(previous_color)
        } else {
            state
                .window_state()
                .image_list_background_colors
                .push((image_list_handle, background_color));

            u64::from(CLR_NONE)
        }
    } else {
        u64::from(CLR_NONE)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ImageList_SetBkColor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `COMCTL32.dll!ImageList_Destroy`.
pub fn handle_image_list_destroy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_Destroy")?;

    let existed = state
        .window_state()
        .image_list_counts
        .iter()
        .any(|(handle, _)| *handle == image_list_handle);

    if existed {
        state
            .window_state()
            .image_list_counts
            .retain(|(handle, _)| *handle != image_list_handle);

        state
            .window_state()
            .image_list_background_colors
            .retain(|(handle, _)| *handle != image_list_handle);
    }

    let return_value = u64::from(existed);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ImageList_Destroy")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `COMCTL32.dll!CreateStatusWindowA`.
pub fn handle_create_status_window_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_status_window(ctx, "CreateStatusWindowA", false)
}

/// Handles `COMCTL32.dll!CreateStatusWindowW`.
pub fn handle_create_status_window_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_status_window(ctx, "CreateStatusWindowW", true)
}

/// Shared `CreateStatusWindowA/W` implementation.
///
/// Signature: `HWND CreateStatusWindow(DWORD style, LPC(WSTR) lpszText,
/// HWND hwndParent, UINT wID)`. Creates the `STATUSCLASSNAMEW` child through
/// the same internal path `CreateWindowExA/W` use (`create_window_record`),
/// forcing `WS_CHILD` onto the caller's style, and returns the new HWND.
/// Parts layout, `SB_*` messages, and full painting are plan Task 3.1.
fn handle_create_status_window(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let style_raw = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let text_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let parent_handle = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let window_id = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    let style =
        u32::try_from(style_raw).with_context(|| format!("{api_name}: style does not fit u32"))?;

    let text = if text_ptr == 0 {
        String::new()
    } else if unicode {
        read_guest_utf16_lossy(engine, text_ptr, 512)
            .with_context(|| format!("failed to read {api_name} text"))?
    } else {
        read_guest_ansi_lossy(engine, text_ptr, 512)
            .with_context(|| format!("failed to read {api_name} text"))?
    };

    let (hwnd, _window_proc, _class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name(STATUSCLASSNAME.to_owned()),
            title: text,
            style: style | WS_CHILD,
            extended_style: 0,
            parent_handle,
            // wID is the child-window identifier (menu_handle slot).
            menu_handle: window_id,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        unicode,
    )
    .with_context(|| format!("failed to create status window for {api_name}"))?;

    let return_address = engine
        .return_from_win64_api(hwnd)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: hwnd,
    })
}
