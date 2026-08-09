use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::gdi32::window_font_resolution_or_default;
use crate::guest_memory::{
    checked_address, read_i32, read_u64 as read_guest_u64, write_u32 as write_guest_u32,
    write_u64 as write_guest_u64,
};
use crate::user32::controls::{
    CCS_BOTTOM, CCS_NOPARENTALIGN, CCS_NORESIZE, ControlClassKind, ControlState, SB_GETPARTS,
    SB_GETTEXTA, SB_GETTEXTLENGTHA, SB_GETTEXTLENGTHW, SB_GETTEXTW, SB_SETPARTS, SB_SETTEXTA,
    SB_SETTEXTW, SBT_NOBORDERS, SBT_OWNERDRAW, SBT_POPOUT,
};
use crate::user32::{
    CreateWindowRequest, WS_CHILD, WinApiState, WindowClassIdentifier, create_window_record,
    find_window, find_window_mut, low_i32, read_guest_ansi_lossy, read_guest_utf16_lossy,
    window_client_size, write_guest_ansi_c_string, write_guest_i32, write_guest_utf16_c_string,
};
use crate::{HandlerContext, WinApiHandlerResult};

const S_OK: u64 = 0;
const FAKE_IMAGE_LIST_HANDLE: u64 = 0x0000_0000_6900_0001;
const CLR_NONE: u32 = 0xffff_ffff;

/// `ToolbarWindow32` — the comctl32 toolbar class name.
///
/// `CreateToolbarEx` always creates the child with this class. Like
/// `STATUSCLASSNAMEW` it is a system class the guest never registers, so the
/// record gets no guest WndProc (a plain window).
const TOOLBAR_CLASS: &str = "ToolbarWindow32";

/// Richer per-image-list record for the string-path ImageList APIs
/// (`ImageList_Add`, `GetImageCount`, `GetIconSize`, `SetIconSize`, `Draw`,
/// `GetImageInfo`).
///
/// The dense table (`ImageList_Create`/`AddMasked`/`Destroy`) tracks only the
/// count in `WindowState::image_list_counts`; this table carries the icon size
/// and per-image HBITMAPs the new handlers need. It is keyed by the fake
/// image-list handle — one record per list, kept in lockstep with the dense
/// count on every create/add/destroy.
#[derive(Debug, Clone, Default)]
struct ImageListRecord {
    /// Stored icon cell width (`ImageList_Create` / `ImageList_SetIconSize`).
    icon_width: i32,
    /// Stored icon cell height.
    icon_height: i32,
    /// One slot per image: the HBITMAP recorded at add time (0 when the add
    /// passed no bitmap).
    images: Vec<u64>,
}

/// The process-global image-list table.
///
/// `WindowState` cannot grow (its fields are owned by the state module), so
/// this table lives here — the same shared-mutable-state seam as
/// `vfs::pick_mount::PICK_MOUNTS`. `HashMap` needs a runtime seed, hence
/// `LazyLock` (plain `Mutex::new(HashMap::new())` is not const).
static IMAGE_LISTS: std::sync::LazyLock<Mutex<HashMap<u64, ImageListRecord>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the image-list table, failing closed (`None`) on a poisoned lock so a
/// panicking thread cannot unwind into the guest.
fn lock_image_lists() -> Option<std::sync::MutexGuard<'static, HashMap<u64, ImageListRecord>>> {
    IMAGE_LISTS.lock().ok()
}

/// Reset the rich record for `himl` to a fresh empty list
/// (`ImageList_Create` semantics — count 0, size from the create args).
fn seed_image_list_record(himl: u64, icon_width: i32, icon_height: i32) {
    if let Ok(mut lists) = IMAGE_LISTS.lock() {
        lists.insert(
            himl,
            ImageListRecord {
                icon_width,
                icon_height,
                images: Vec::new(),
            },
        );
    }
}

/// `WM_SIZE` — the message the status bar intercepts to reposition itself.
const WM_SIZE_MSG: u32 = crate::user32::wm::WinMsg::WM_SIZE.as_u32();

/// Cap for the `SB_GETTEXTW`/`SB_GETTEXTA` guest output buffers (the message
/// has no size argument — the guest is expected to provide a big-enough
/// buffer, so a fixed generous cap bounds the write).
const SB_GETTEXT_CAP: usize = 1024;

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
    let version_info_va = engine
        .read_rcx()
        .context("failed to read RCX for DllGetVersion")?;

    if version_info_va != 0 {
        // DLLVERSIONINFO:
        // DWORD cbSize;          offset 0
        // DWORD dwMajorVersion;  offset 4
        // DWORD dwMinorVersion;  offset 8
        // DWORD dwBuildNumber;   offset 12
        // DWORD dwPlatformID;    offset 16
        //
        // Common Controls v6-ish fake version.
        write_guest_u32(engine, version_info_va, 20)?;
        write_guest_u32(
            engine,
            checked_address(version_info_va, 4, "dwMajorVersion"),
            6,
        )?;
        write_guest_u32(
            engine,
            checked_address(version_info_va, 8, "dwMinorVersion"),
            0,
        )?;
        write_guest_u32(
            engine,
            checked_address(version_info_va, 12, "dwBuildNumber"),
            7600,
        )?;
        write_guest_u32(
            engine,
            checked_address(version_info_va, 16, "dwPlatformID"),
            1,
        )?;
    }

    ctx.finish(S_OK)
}

/// Handles `COMCTL32.dll!InitCommonControls` imported as ordinal 17.
pub fn handle_init_common_controls(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}

/// Handles dynamic `COMCTL32.dll!InitCommonControlsEx`.
pub fn handle_init_common_controls_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let init_common_controls_ex_va = engine
        .read_rcx()
        .context("failed to read RCX for InitCommonControlsEx")?;

    let return_value = u64::from(init_common_controls_ex_va != 0);

    ctx.finish(return_value)
}

/// Handles `COMCTL32.dll!ImageList_Create`.
pub fn handle_image_list_create(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let icon_width = low_i32(
        engine
            .read_rcx()
            .context("failed to read RCX for ImageList_Create")?,
        "ImageList_Create icon width",
    )?;

    let icon_height = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for ImageList_Create")?,
        "ImageList_Create icon height",
    )?;

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

    // Seed the rich record (icon size + image slots) so the string-path
    // ImageList handlers (ImageList_Add/GetImageInfo/…) see a fresh list.
    seed_image_list_record(FAKE_IMAGE_LIST_HANDLE, icon_width, icon_height);

    ctx.finish(FAKE_IMAGE_LIST_HANDLE)
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

        // Keep the rich record in lockstep so ImageList_GetImageInfo returns
        // the stored HBITMAP for the new slot.
        if let Ok(mut lists) = IMAGE_LISTS.lock()
            && let Some(record) = lists.get_mut(&image_list_handle)
        {
            record.images.push(bitmap_handle);
        }

        image_index
    } else {
        u64::MAX
    };

    ctx.finish(return_value)
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

    ctx.finish(return_value)
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

        if let Ok(mut lists) = IMAGE_LISTS.lock() {
            lists.remove(&image_list_handle);
        }
    }

    let return_value = u64::from(existed);

    ctx.finish(return_value)
}

// ── String-path ImageList APIs (Tier-2) ─────────────────────────────────
//
// These run through `dispatch_comctl32_extra` (no `WinApiId` rows). The
// rich record above (`IMAGE_LISTS`) carries the icon size and per-image
// HBITMAPs; the dense `WindowState` table stays the count's home, and every
// handler here keeps the two in lockstep.

/// Handles `COMCTL32.dll!ImageList_Add`.
///
/// Signature: `int ImageList_Add(HIMAGELIST himl, HBITMAP hbmImage,
/// HBITMAP hbmMask)`. Appends one slot to the rich record and returns its
/// 0-based index (`-1` = `u64::MAX` on failure). The HBITMAP is stored by
/// handle — `ImageList_GetImageInfo` returns it verbatim; the pixels are not
/// copied (there is no DIB snapshot for `ImageList_Draw`).
pub fn handle_image_list_add(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_Add")?;
    let bitmap_handle = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_Add")?;
    let _mask_handle = engine
        .read_r8()
        .context("failed to read R8 for ImageList_Add")?;

    let registered = state
        .window_state()
        .image_list_counts
        .iter()
        .any(|(handle, _)| *handle == image_list_handle);

    let image_index = if image_list_handle == FAKE_IMAGE_LIST_HANDLE && registered {
        let Some(mut lists) = lock_image_lists() else {
            // Poisoned lock: fail closed, no index handed out.
            return ctx.finish(u64::MAX);
        };
        let record = lists.entry(image_list_handle).or_default();
        let index = u64::try_from(record.images.len()).context("image list index overflow")?;
        record.images.push(bitmap_handle);
        if let Some((_, count)) = state
            .window_state()
            .image_list_counts
            .iter_mut()
            .find(|(handle, _)| *handle == image_list_handle)
        {
            *count = index.saturating_add(1);
        }
        index
    } else {
        u64::MAX
    };

    ctx.finish(image_index)
}

/// Handles `COMCTL32.dll!ImageList_GetImageCount`.
///
/// Signature: `int ImageList_GetImageCount(HIMAGELIST himl)`. Reads the dense
/// `WindowState` count — both add paths (`ImageList_Add` and the dense
/// `ImageList_AddMasked`) keep it in lockstep with the rich record.
pub fn handle_image_list_get_image_count(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_GetImageCount")?;

    let count = state
        .window_state()
        .image_list_counts
        .iter()
        .find(|(handle, _)| *handle == image_list_handle)
        .map_or(0, |(_, count)| *count);

    ctx.finish(count)
}

/// Handles `COMCTL32.dll!ImageList_GetIconSize`.
///
/// Signature: `BOOL ImageList_GetIconSize(HIMAGELIST himl, int *cx, int *cy)`.
/// Writes the stored cell size (0s when the list is unknown) and returns
/// TRUE.
pub fn handle_image_list_get_icon_size(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_GetIconSize")?;
    let cx_va = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_GetIconSize")?;
    let cy_va = engine
        .read_r8()
        .context("failed to read R8 for ImageList_GetIconSize")?;

    let (width, height) = lock_image_lists()
        .and_then(|lists| lists.get(&image_list_handle).cloned())
        .map_or((0, 0), |record| (record.icon_width, record.icon_height));

    if cx_va != 0 {
        write_guest_i32(engine, cx_va, width)?;
    }
    if cy_va != 0 {
        write_guest_i32(engine, cy_va, height)?;
    }

    ctx.finish(1)
}

/// Handles `COMCTL32.dll!ImageList_SetIconSize`.
///
/// Signature: `BOOL ImageList_SetIconSize(HIMAGELIST himl, int cx, int cy)`.
/// Stores the cell size on the rich record (no-op for unknown lists) and
/// returns TRUE.
pub fn handle_image_list_set_icon_size(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_SetIconSize")?;
    let cx = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for ImageList_SetIconSize")?,
        "ImageList_SetIconSize cx",
    )?;
    let cy = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for ImageList_SetIconSize")?,
        "ImageList_SetIconSize cy",
    )?;

    let registered = state
        .window_state()
        .image_list_counts
        .iter()
        .any(|(handle, _)| *handle == image_list_handle);

    if image_list_handle == FAKE_IMAGE_LIST_HANDLE
        && registered
        && let Some(mut lists) = lock_image_lists()
    {
        let record = lists.entry(image_list_handle).or_default();
        record.icon_width = cx;
        record.icon_height = cy;
    }

    ctx.finish(1)
}

/// Handles `COMCTL32.dll!ImageList_Draw`.
///
/// Signature: `BOOL ImageList_Draw(HIMAGELIST himl, int i, HDC hdcDst, int x,
/// int y, UINT fStyle)`. Documented no-op: the rich record stores HBITMAP
/// handles, not DIB snapshots, so there is nothing to blit — the honest blit
/// path (`gdi32` `BitBlt`) needs a source DIB we deliberately do not keep for
/// this milestone.
pub fn handle_image_list_draw(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_Draw")?;
    let _image_index = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_Draw")?;
    let _target_dc = engine
        .read_r8()
        .context("failed to read R8 for ImageList_Draw")?;
    let _x = engine
        .read_r9()
        .context("failed to read R9 for ImageList_Draw")?;

    tracing::debug!(
        target: "wie_winapi",
        himl = format_args!("{image_list_handle:#x}"),
        "ImageList_Draw: documented no-op (no DIB snapshot)"
    );

    ctx.finish(1)
}

/// Handles `COMCTL32.dll!ImageList_GetImageInfo`.
///
/// Signature: `BOOL ImageList_GetImageInfo(HIMAGELIST himl, int i,
/// IMAGEINFO *pImageInfo)`. Fills `IMAGEINFO` with the stored HBITMAP for
/// slot `i` and the cell size. Layout verified against the mingw `commctrl.h`
/// header (NOT the task sheet, whose rcImage offset of 32 was wrong):
/// hbmImage@0, hbmMask@8, Unused1@16, Unused2@20, RECT rcImage@24.
pub fn handle_image_list_get_image_info(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let image_list_handle = engine
        .read_rcx()
        .context("failed to read RCX for ImageList_GetImageInfo")?;
    let image_index_raw = engine
        .read_rdx()
        .context("failed to read RDX for ImageList_GetImageInfo")?;
    let image_info_va = engine
        .read_r8()
        .context("failed to read R8 for ImageList_GetImageInfo")?;

    if image_info_va == 0 {
        return ctx.finish(0);
    }

    let image_index = usize::try_from(image_index_raw).unwrap_or(usize::MAX);

    let (stored_bitmap, icon_width, icon_height) = lock_image_lists()
        .and_then(|lists| lists.get(&image_list_handle).cloned())
        .map_or((0, 0, 0), |record| {
            (
                record.images.get(image_index).copied().unwrap_or(0),
                record.icon_width,
                record.icon_height,
            )
        });

    write_guest_u64(
        engine,
        checked_address(image_info_va, 0, "IMAGEINFO hbmImage"),
        stored_bitmap,
    )?;
    write_guest_u64(
        engine,
        checked_address(image_info_va, 8, "IMAGEINFO hbmMask"),
        0,
    )?;
    write_guest_u32(
        engine,
        checked_address(image_info_va, 16, "IMAGEINFO Unused1"),
        0,
    )?;
    write_guest_u32(
        engine,
        checked_address(image_info_va, 20, "IMAGEINFO Unused2"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(image_info_va, 24, "IMAGEINFO rcImage.left"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(image_info_va, 28, "IMAGEINFO rcImage.top"),
        0,
    )?;
    write_guest_i32(
        engine,
        checked_address(image_info_va, 32, "IMAGEINFO rcImage.right"),
        icon_width,
    )?;
    write_guest_i32(
        engine,
        checked_address(image_info_va, 36, "IMAGEINFO rcImage.bottom"),
        icon_height,
    )?;

    ctx.finish(1)
}

/// Handles `COMCTL32.dll!CreateStatusWindowA`.
pub fn handle_create_status_window_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_status_window_impl(ctx, "CreateStatusWindowA", false)
}

/// Handles `COMCTL32.dll!CreateStatusWindowW`.
pub fn handle_create_status_window_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_status_window_impl(ctx, "CreateStatusWindowW", true)
}

/// Shared `CreateStatusWindowA/W` implementation.
///
/// Signature: `HWND CreateStatusWindow(DWORD style, LPC(WSTR) lpszText,
/// HWND hwndParent, UINT wID)`. Creates the `STATUSCLASSNAMEW` child through
/// the same internal path `CreateWindowExA/W` use (`create_window_record`),
/// forcing `WS_CHILD` onto the caller's style, and returns the new HWND. The
/// creation text lands on the window record (`control_text`) and reads as
/// part 0's text until an `SB_SETTEXTW(0, …)` overwrites it (Task 3.1).
fn handle_create_status_window_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let style_raw = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let text_va = engine
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

    let text = if text_va == 0 {
        String::new()
    } else if unicode {
        read_guest_utf16_lossy(engine, text_va, 512)
            .with_context(|| format!("failed to read {api_name} text"))?
    } else {
        read_guest_ansi_lossy(engine, text_va, 512)
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

    ctx.finish(hwnd)
}

// ── Task 3.1: the STATUSCLASSNAMEW control's own message dispatch ────────
//
// The generic control arms in `dispatch_control_proc` (WM_PAINT, WM_GETTEXT,
// WM_SETTEXT, WM_SETFONT, …) run first; `dispatch_status_bar_message` handles
// what falls through: the comctl32 SB_* messages (WM_USER+ offsets that
// `WinMsg` cannot name) and WM_SIZE, which repositions the bar inside its
// parent at the default font-derived height.

/// The status-bar state for `hwnd`, seeded on first touch.
///
/// The generic seeder (`ControlClassKind::new_state`) leaves `part_texts`
/// empty; this seeder additionally folds the `CreateStatusWindowA/W` text
/// into part 0 — real comctl32 applies that text as `SB_SETTEXT(0, …)`
/// internally, so part 0 shows it until a guest overwrites part 0.
fn status_state_mut(state: &mut WinApiState, hwnd: u64) -> Option<&mut ControlState> {
    let ws = state.window_state();
    let seed_text = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map(|w| w.control_text.clone());
    let control = ws
        .control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::StatusBar.new_state());
    if let ControlState::StatusBar { part_texts, .. } = control
        && part_texts.is_empty()
        && let Some(text) = seed_text
    {
        part_texts.push(text);
    }
    Some(control)
}

/// The text of status-bar `part`: the stored per-part text, or — for part 0
/// with nothing stored yet — the window's `control_text` (the
/// `CreateStatusWindowA/W` text, which real comctl32 applies to part 0).
/// `pub(crate)` so the status-bar paint (controls/button.rs) reads the same
/// resolved text the SB_GETTEXT* handlers return.
#[must_use]
pub(crate) fn status_part_text(state: &WinApiState, hwnd: u64, part: usize) -> String {
    let stored = match state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
    {
        Some(ControlState::StatusBar { part_texts, .. }) => part_texts.get(part).cloned(),
        _ => None,
    };
    match stored {
        Some(text) => text,
        None if part == 0 => state
            .try_window_state()
            .and_then(|ws| {
                ws.windows
                    .iter()
                    .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            })
            .map_or_else(String::new, |w| w.control_text.clone()),
        None => String::new(),
    }
}

/// Mark the bar for a future synthesized WM_PAINT after an SB_* mutation.
fn status_bar_invalidate(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.invalidated = true;
    }
}

/// Host-side message dispatch for a STATUSCLASSNAMEW status-bar window.
pub(crate) fn dispatch_status_bar_message(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    match message {
        WM_SIZE_MSG => {
            // WM_SIZE to a status bar repositions it in its parent (notepad
            // sends SendMessageW(hStatusBar, WM_SIZE, 0, 0) after creating
            // the bar, then reads the resulting rect to size the EDIT).
            status_bar_reposition(state, hwnd)?;
            Ok(Some(0))
        }
        // SB_SETPARTS: wParam = part count, lParam = int array of right-edge
        // coordinates (client coords); -1 = extend to the right edge.
        SB_SETPARTS => {
            let count = usize::try_from(word_parameter).unwrap_or(0);
            let mut rights = Vec::with_capacity(count);
            for index in 0..count {
                let off = u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(4);
                let value = if long_parameter == 0 {
                    -1 // no array: the whole strip is one part
                } else {
                    read_i32(engine, long_parameter.saturating_add(off))
                        .context("SB_SETPARTS part width read failed")?
                };
                rights.push(value);
            }
            if let Some(ControlState::StatusBar { part_rights, .. }) = status_state_mut(state, hwnd)
            {
                *part_rights = rights;
            }
            status_bar_invalidate(state, hwnd);
            Ok(Some(1)) // TRUE
        }
        // SB_SETTEXTW (WM_USER+11): wParam = part | SBT_* flags (the part is
        // the low byte; the SBT_NOBORDERS/POPOUT/OWNERDRAW bits are masked
        // off and ignored), lParam = wide string. Modern comctl32 splits
        // SB_SETTEXT into A/W variants — notepad sends the W variant (0x040B)
        // with UTF-16 text.
        SB_SETTEXTW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let _flags = word_parameter
                & (u64::from(SBT_NOBORDERS) | u64::from(SBT_POPOUT) | u64::from(SBT_OWNERDRAW));
            let text = if long_parameter == 0 {
                String::new()
            } else {
                read_guest_utf16_lossy(engine, long_parameter, 32_768)
                    .context("SB_SETTEXTW text read failed")?
            };
            tracing::debug!(
                target: "wie_winapi",
                hwnd = format_args!("{hwnd:#x}"),
                part,
                text = %text,
                "SB_SETTEXTW"
            );
            status_bar_set_text(state, hwnd, part, text);
            Ok(Some(1)) // TRUE
        }
        // SB_SETTEXTA (WM_USER+1, the legacy pre-v6 single-variant value too):
        // same message with an ANSI string.
        SB_SETTEXTA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let _flags = word_parameter
                & (u64::from(SBT_NOBORDERS) | u64::from(SBT_POPOUT) | u64::from(SBT_OWNERDRAW));
            let text = if long_parameter == 0 {
                String::new()
            } else {
                read_guest_ansi_lossy(engine, long_parameter, 32_768)
                    .context("SB_SETTEXTA text read failed")?
            };
            tracing::debug!(
                target: "wie_winapi",
                hwnd = format_args!("{hwnd:#x}"),
                part,
                text = %text,
                "SB_SETTEXTA"
            );
            status_bar_set_text(state, hwnd, part, text);
            Ok(Some(1)) // TRUE
        }
        // SB_GETPARTS: wParam = max parts to copy, lParam = int buffer;
        // returns the part count.
        SB_GETPARTS => {
            let rights = match state
                .try_window_state()
                .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
            {
                Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
                _ => Vec::new(),
            };
            // No parts configured = one implicit part spanning the strip.
            let count = if rights.is_empty() { 1 } else { rights.len() };
            if long_parameter != 0 {
                let max = usize::try_from(word_parameter).unwrap_or(0).min(count);
                for index in 0..max {
                    let value = if rights.is_empty() {
                        -1 // the implicit single part extends to the edge
                    } else {
                        rights.get(index).copied().unwrap_or(-1)
                    };
                    let off = u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(4);
                    write_guest_i32(engine, long_parameter.saturating_add(off), value)
                        .context("SB_GETPARTS width write failed")?;
                }
            }
            Ok(Some(u64::try_from(count).unwrap_or(0)))
        }
        // SB_GETTEXTW: wParam = part, lParam = WCHAR buffer; returns the
        // char count (excluding the NUL).
        SB_GETTEXTW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            let copied = write_guest_utf16_c_string(engine, long_parameter, SB_GETTEXT_CAP, &text)
                .context("SB_GETTEXTW buffer write failed")?;
            Ok(Some(u64::try_from(copied).unwrap_or(0)))
        }
        // SB_GETTEXTA: same with an ANSI buffer.
        SB_GETTEXTA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            let copied = write_guest_ansi_c_string(engine, long_parameter, SB_GETTEXT_CAP, &text)
                .context("SB_GETTEXTA buffer write failed")?;
            Ok(Some(u64::try_from(copied).unwrap_or(0)))
        }
        // SB_GETTEXTLENGTHW / SB_GETTEXTLENGTHA: the stored text length in
        // the message's units (WCHARs for W, bytes for A).
        SB_GETTEXTLENGTHW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            Ok(Some(
                u64::try_from(text.encode_utf16().count()).unwrap_or(0),
            ))
        }
        SB_GETTEXTLENGTHA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            Ok(Some(u64::try_from(text.len()).unwrap_or(0)))
        }
        _ => Ok(None),
    }
}

/// Store a per-part text (SB_SETTEXTW/SB_SETTEXTA), extending the part vec
/// with empty cells so `part` is always addressable.
fn status_bar_set_text(state: &mut WinApiState, hwnd: u64, part: usize, text: String) {
    if let Some(ControlState::StatusBar { part_texts, .. }) = status_state_mut(state, hwnd) {
        if part >= part_texts.len() {
            part_texts.resize(part.saturating_add(1), String::new());
        }
        if let Some(slot) = part_texts.get_mut(part) {
            *slot = text;
        }
    }
    status_bar_invalidate(state, hwnd);
}

/// Reposition the bar inside its parent — the WM_SIZE behavior real comctl32
/// gives the built-in class: full parent width at the default font-derived
/// height, flush at the parent bottom (CCS_BOTTOM) or top (CCS_TOP), unless
/// CCS_NOPARENTALIGN / CCS_NORESIZE opt out.
fn status_bar_reposition(state: &mut WinApiState, hwnd: u64) -> Result<()> {
    let (parent_handle, style) = find_window(state, hwnd)
        .map(|w| (w.parent_handle, w.style))
        .context("status bar record missing for WM_SIZE")?;
    if parent_handle == crate::handles::Hwnd::NULL {
        return Ok(());
    }
    let (parent_w, parent_h) = window_client_size(state, parent_handle.as_u64());
    let height = status_bar_default_height(state, hwnd)?;
    let width = if style & CCS_NORESIZE != 0 {
        find_window(state, hwnd).map_or(0, |w| w.width)
    } else {
        parent_w
    };
    let (x, y) = if style & CCS_NOPARENTALIGN != 0 {
        // The guest placed the bar itself; leave its position alone.
        find_window(state, hwnd).map_or((0, 0), |w| (w.x, w.y))
    } else if style & CCS_BOTTOM != 0 {
        // notepad's bar: flush at the parent's bottom edge.
        (0, parent_h.saturating_sub(height))
    } else {
        // CCS_TOP (and the unaligned default): flush at the top edge.
        (0, 0)
    };
    if let Some(window) = find_window_mut(state, hwnd) {
        window.x = x;
        window.y = y;
        window.width = width;
        window.height = height;
        window.invalidated = true;
    }
    Ok(())
}

/// The bar's default height: the stored control font's line height (the
/// system default when none is set) plus the two client-edge border rows.
fn status_bar_default_height(state: &mut WinApiState, hwnd: u64) -> Result<i32> {
    let line_h = state.with_font_engine(|state, font_engine| {
        window_font_resolution_or_default(state, hwnd, font_engine)
            .map_or(16, |(_key, resolved)| resolved.line_height())
    });
    Ok(line_h.saturating_add(4))
}

/// Handles `COMCTL32.dll!CreateToolbarEx` (string-path).
///
/// Signature (mingw `commctrl.h`, 13 args): `CreateToolbarEx(HWND hwnd,
/// DWORD ws, UINT wID, int nBitmaps, HINSTANCE hBMInst, UINT_PTR wBMID,
/// LPCTBBUTTON lpButtons, int iNumButtons, int dxButton, int dyButton,
/// int dxBitmap, int dyBitmap, UINT uStructSize)`. Creates the
/// `ToolbarWindow32` child through the same internal path
/// `CreateStatusWindowA/W` use (`create_window_record`), forcing `WS_CHILD`
/// onto the caller's style, and returns the new HWND (0 on failure).
pub fn handle_create_toolbar_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_handle = engine
        .read_rcx()
        .context("failed to read RCX for CreateToolbarEx")?;
    let style_raw = engine
        .read_rdx()
        .context("failed to read RDX for CreateToolbarEx")?;
    let window_id = engine
        .read_r8()
        .context("failed to read R8 for CreateToolbarEx")?;
    let _bitmap_count = engine
        .read_r9()
        .context("failed to read R9 for CreateToolbarEx")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateToolbarEx")?;
    // Args 5..13 live on the stack at 0x28 upward (Win64 ABI). Only the
    // button cell size feeds the window record; the bitmap/button arrays are
    // out of scope for the milestone.
    let _bitmap_instance = read_guest_u64(
        engine,
        checked_address(rsp, 0x28, "CreateToolbarEx hBMInst"),
    )
    .context("failed to read CreateToolbarEx hBMInst")?;
    let _bitmap_id = read_guest_u64(engine, checked_address(rsp, 0x30, "CreateToolbarEx wBMID"))
        .context("failed to read CreateToolbarEx wBMID")?;
    let _buttons_va = read_guest_u64(
        engine,
        checked_address(rsp, 0x38, "CreateToolbarEx lpButtons"),
    )
    .context("failed to read CreateToolbarEx lpButtons")?;
    let _button_count = read_guest_u64(
        engine,
        checked_address(rsp, 0x40, "CreateToolbarEx iNumButtons"),
    )
    .context("failed to read CreateToolbarEx iNumButtons")?;
    let button_width = read_i32(
        engine,
        checked_address(rsp, 0x48, "CreateToolbarEx dxButton"),
    )
    .context("failed to read CreateToolbarEx dxButton")?;
    let button_height = read_i32(
        engine,
        checked_address(rsp, 0x50, "CreateToolbarEx dyButton"),
    )
    .context("failed to read CreateToolbarEx dyButton")?;
    let _bitmap_width = read_i32(
        engine,
        checked_address(rsp, 0x58, "CreateToolbarEx dxBitmap"),
    )
    .context("failed to read CreateToolbarEx dxBitmap")?;
    let _bitmap_height = read_i32(
        engine,
        checked_address(rsp, 0x60, "CreateToolbarEx dyBitmap"),
    )
    .context("failed to read CreateToolbarEx dyBitmap")?;
    let _struct_size = read_guest_u64(
        engine,
        checked_address(rsp, 0x68, "CreateToolbarEx uStructSize"),
    )
    .context("failed to read CreateToolbarEx uStructSize")?;

    let style = u32::try_from(style_raw).context("CreateToolbarEx style does not fit u32")?;

    let (toolbar_handle, _window_proc, _class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name(TOOLBAR_CLASS.to_owned()),
            title: String::new(),
            style: style | WS_CHILD,
            extended_style: 0,
            parent_handle,
            // wID is the child-window identifier (menu_handle slot).
            menu_handle: window_id,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: button_width,
            height: button_height,
        },
        false,
    )
    .context("failed to create toolbar window for CreateToolbarEx")?;

    ctx.finish(toolbar_handle)
}

/// Soft dispatch for COMCTL32 exports beyond the dense table (toolbar,
/// remaining ImageList APIs, progress bar). String path only.
pub fn dispatch_comctl32_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "imagelist_add" => Ok(Some(handle_image_list_add(ctx)?)),
        "imagelist_getimagecount" => Ok(Some(handle_image_list_get_image_count(ctx)?)),
        "imagelist_geticonsize" => Ok(Some(handle_image_list_get_icon_size(ctx)?)),
        "imagelist_seticonsize" => Ok(Some(handle_image_list_set_icon_size(ctx)?)),
        "imagelist_draw" => Ok(Some(handle_image_list_draw(ctx)?)),
        "imagelist_getimageinfo" => Ok(Some(handle_image_list_get_image_info(ctx)?)),
        // mingw declares CreateToolbarEx without an A/W split; keep the
        // suffixed names as a defensive match for other toolchains.
        "createtoolbarex" | "createtoolbarexa" | "createtoolbarexw" => {
            Ok(Some(handle_create_toolbar_ex(ctx)?))
        }
        _ => Ok(None),
    }
}
