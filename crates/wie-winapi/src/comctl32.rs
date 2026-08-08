use anyhow::{Context, Result};

use crate::gdi32::window_font_resolution_or_default;
use crate::guest_memory::{checked_address, read_i32, write_u32 as write_guest_u32};
use crate::user32::controls::{
    CCS_BOTTOM, CCS_NOPARENTALIGN, CCS_NORESIZE, ControlClassKind, ControlState, SB_GETPARTS,
    SB_GETTEXTA, SB_GETTEXTLENGTHA, SB_GETTEXTLENGTHW, SB_GETTEXTW, SB_SETPARTS, SB_SETTEXTA,
    SB_SETTEXTW, SBT_NOBORDERS, SBT_OWNERDRAW, SBT_POPOUT,
};
use crate::user32::{
    CreateWindowRequest, WS_CHILD, WinApiState, WindowClassIdentifier, create_window_record,
    find_window, find_window_mut, read_guest_ansi_lossy, read_guest_utf16_lossy,
    window_client_size, write_guest_ansi_c_string, write_guest_i32, write_guest_utf16_c_string,
};
use crate::{HandlerContext, WinApiHandlerResult};

const S_OK: u64 = 0;
const FAKE_IMAGE_LIST_HANDLE: u64 = 0x0000_0000_6900_0001;
const CLR_NONE: u32 = 0xffff_ffff;

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
    }

    let return_value = u64::from(existed);

    ctx.finish(return_value)
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

/// Soft dispatch for COMCTL32 exports beyond the dense table (toolbar,
/// remaining ImageList APIs, progress bar). String path only.
pub fn dispatch_comctl32_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = (ctx, name);
    Ok(None)
}
