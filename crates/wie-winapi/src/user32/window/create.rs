//! Window creation: `CreateWindowExA/W` with the in-guest `CREATESTRUCT` build
//! (split from the former `window.rs`).

use super::class::find_window_mut;
use crate::OuterReturn;
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::CreateStruct;
use crate::user32::{
    CreateWindowRequest, GuestCallbackRequest, HandlerContext, Result, WM_CREATE,
    WinApiControlSignal, WinApiHandlerResult, create_window_record, read_guest_ansi_lossy,
    read_guest_utf16_lossy, read_i32, read_u64, read_window_class_identifier_a,
    read_window_class_identifier_w, with_typed_write,
};
use anyhow::Context as _;

/// `CW_USEDEFAULT` (winuser.h 0x8000_0000) — the "use the default geometry"
/// sentinel a guest passes for a CreateWindowEx x/y/cx/cy argument. Stored as
/// a 32-bit DWORD on the stack, it reads back as `i32::MIN`.
const CW_USEDEFAULT: i32 = i32::MIN;
/// `CW_USEDEFAULT` top-level origin: the cascaded (100, 100) position (each
/// new window steps 100 px down/right from the previous one).
const CW_USEDEFAULT_ORIGIN: i32 = 100;
/// `CW_USEDEFAULT` top-level size: 640×480.
const CW_USEDEFAULT_WIDTH: i32 = 640;
const CW_USEDEFAULT_HEIGHT: i32 = 480;

/// Handles `USER32.dll!CreateWindowExA`.
pub fn handle_create_window_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_window_ex_impl(ctx, false)
}

/// Handles `USER32.dll!CreateWindowExW`.
pub fn handle_create_window_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_window_ex_impl(ctx, true)
}

/// Shared `CreateWindowExA/W` implementation.
///
/// Win64 ABI: `rcx` = dwExStyle, `rdx` = class, `r8` = window title,
/// `r9` = style; the remaining eight arguments are on the stack. Builds the
/// window record, registers top-level windows with the presenter, and queues
/// the guest `WM_CREATE` callback with an in-guest `CREATESTRUCT`.
fn create_window_ex_impl(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let api_name = if wide {
        "CreateWindowExW"
    } else {
        "CreateWindowExA"
    };

    // Read 4 register args
    let ex_style_raw = read_arg(engine, ArgReg::Rcx, api_name)?;
    let class_value = read_arg(engine, ArgReg::Rdx, api_name)?;
    let window_title = read_arg(engine, ArgReg::R8, api_name)?;
    let style_raw = read_arg(engine, ArgReg::R9, api_name)?;

    // Read 8 stack args
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;

    let stack_arg = |offset: u64, name: &str| -> Result<u64> {
        rsp.checked_add(offset)
            .with_context(|| format!("{api_name}: {name} address overflow"))
    };

    let x_raw = read_i32(engine, stack_arg(0x28, "X")?)?;
    let y_raw = read_i32(engine, stack_arg(0x30, "Y")?)?;
    let width_raw = read_i32(engine, stack_arg(0x38, "nWidth")?)?;
    let height_raw = read_i32(engine, stack_arg(0x40, "nHeight")?)?;
    let parent_handle = read_u64(engine, stack_arg(0x48, "hWndParent")?)?;
    let menu_handle = read_u64(engine, stack_arg(0x50, "hMenu")?)?;
    let instance_handle = read_u64(engine, stack_arg(0x58, "hInstance")?)?;
    let create_params = read_u64(engine, stack_arg(0x60, "lpParam")?)?;

    // Read window title if present (UTF-16 for the W variant).
    let title = if window_title == 0 {
        String::new()
    } else if wide {
        read_guest_utf16_lossy(engine, window_title, 512)
            .with_context(|| format!("failed to read {api_name} window title"))?
    } else {
        read_guest_ansi_lossy(engine, window_title, 512)
            .with_context(|| format!("failed to read {api_name} window title"))?
    };

    // Convert style/ex_style to u32 once (avoids repeated `as` conversions).
    let style =
        u32::try_from(style_raw).with_context(|| format!("{api_name}: style does not fit u32"))?;
    let ex_style = u32::try_from(ex_style_raw)
        .with_context(|| format!("{api_name}: ex_style does not fit u32"))?;

    // Handle CW_USEDEFAULT — stored as a 32-bit DWORD in the stack slot, so
    // it reads back as i32::MIN through read_guest_i32. A CHILD window with
    // CW_USEDEFAULT x/y is placed at (0,0) of the parent's client area; only
    // a top-level window gets the cascaded origin.
    let x = if x_raw == CW_USEDEFAULT {
        if parent_handle != 0 {
            0
        } else {
            CW_USEDEFAULT_ORIGIN
        }
    } else {
        x_raw
    };
    let y = if y_raw == CW_USEDEFAULT {
        if parent_handle != 0 {
            0
        } else {
            CW_USEDEFAULT_ORIGIN
        }
    } else {
        y_raw
    };
    let width = if width_raw == CW_USEDEFAULT {
        CW_USEDEFAULT_WIDTH
    } else {
        width_raw
    };
    let height = if height_raw == CW_USEDEFAULT {
        CW_USEDEFAULT_HEIGHT
    } else {
        height_raw
    };

    let class_identifier = if wide {
        read_window_class_identifier_w(engine, class_value)
    } else {
        read_window_class_identifier_a(engine, class_value)
    }
    .with_context(|| format!("failed to read window class identifier for {api_name}"))?;

    let (hwnd, window_proc, class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style,
            extended_style: ex_style,
            parent_handle,
            menu_handle,
            instance_handle,
            x,
            y,
            width,
            height,
        },
        wide,
    )
    .with_context(|| format!("failed to create window record for {api_name}"))?;

    if hwnd == 0 {
        return ctx.finish(0);
    }

    // A new top-level window (no parent) enters the host-visible window set
    // and the top of the guest z-order. The revision fingerprint makes the
    // presenter's Frame handler reconcile exactly on this create — and on no
    // other frame (the reconcile-on-change latch). Children composite into
    // their parent's surface and own no host window, so only parentless
    // windows register.
    if parent_handle == 0 {
        state
            .present()
            .register_top_level(crate::handles::Hwnd::from(hwnd));
    }

    // Update the window record with client rect
    if let Some(window) = find_window_mut(state, hwnd) {
        window.client_rect = (0, 0, width, height);
    }

    // If there is a window procedure, send WM_CREATE via guest callback
    if window_proc != 0 {
        // Allocate CREATESTRUCT in guest memory (0x50 bytes)
        let cs_va = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alloc_coherent(engine, 0x50);
        if cs_va == 0 {
            // Allocation failed — return HWND without WM_CREATE
            return ctx.finish(hwnd);
        }

        // One shared-lock borrow instead of eleven per-field writes. The
        // CREATESTRUCT layout + pinned offsets (dwExStyle @0x48) live in
        // `crate::guest_layout::CreateStruct`; the zero-fill covers both
        // alignment pads. Both variants share the struct layout; only the
        // pointed-to strings differ in encoding.
        with_typed_write::<CreateStruct, _, _>(engine, cs_va, |cs| {
            cs.create_params = create_params;
            cs.instance_handle = instance_handle;
            cs.menu_handle = menu_handle;
            cs.parent_handle = parent_handle;
            cs.cy = height;
            cs.cx = width;
            cs.y = y;
            cs.x = x;
            cs.style = style;
            cs.name_ptr = window_title;
            cs.class_ptr = class_value;
            cs.extended_style = ex_style;
            Ok(())
        })
        .with_context(|| format!("failed to write CREATESTRUCT for {api_name}"))?;

        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: window_proc,
                window_handle: hwnd,
                message: WM_CREATE,
                word_parameter: 0,
                long_parameter: cs_va,
                unicode: class_unicode,
                outer_return: OuterReturn::CreateWindow(hwnd),
            },
        }
        .into());
    }

    // No window procedure — return the HWND directly
    ctx.finish(hwnd)
}
