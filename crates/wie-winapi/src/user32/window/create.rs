//! Window creation: `CreateWindowExA/W` with the in-guest `CREATESTRUCT` build
//! (split from the former `window.rs`).

use super::class::find_window_mut;
use crate::OuterReturn;
use crate::guest_layout::CreateStruct;
use crate::user32::{
    Context, CreateWindowRequest, GuestCallbackRequest, HandlerContext, Result, WM_CREATE,
    WinApiControlSignal, WinApiHandlerResult, create_window_record, read_guest_ansi_lossy,
    read_guest_i32, read_guest_u64, read_guest_utf16_lossy, read_window_class_identifier_a,
    read_window_class_identifier_w, with_typed_write,
};

/// Handles `USER32.dll!CreateWindowExA`.
pub fn handle_create_window_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Read 4 register args
    let ex_style = engine
        .read_rcx()
        .context("failed to read RCX for CreateWindowExA")?;
    let class_value = engine
        .read_rdx()
        .context("failed to read RDX for CreateWindowExA")?;
    let window_title = engine
        .read_r8()
        .context("failed to read R8 for CreateWindowExA")?;
    let style_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateWindowExA")?;

    // Read 8 stack args
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateWindowExA")?;

    let stack_arg = |offset: u64, name: &str| -> Result<u64> {
        rsp.checked_add(offset)
            .with_context(|| format!("CreateWindowExA: {name} address overflow"))
    };

    let x_raw = read_guest_i32(engine, stack_arg(0x28, "X")?)?;
    let y_raw = read_guest_i32(engine, stack_arg(0x30, "Y")?)?;
    let width_raw = read_guest_i32(engine, stack_arg(0x38, "nWidth")?)?;
    let height_raw = read_guest_i32(engine, stack_arg(0x40, "nHeight")?)?;
    let parent_handle = read_guest_u64(engine, stack_arg(0x48, "hWndParent")?)?;
    let menu_handle = read_guest_u64(engine, stack_arg(0x50, "hMenu")?)?;
    let instance_handle = read_guest_u64(engine, stack_arg(0x58, "hInstance")?)?;
    let create_params = read_guest_u64(engine, stack_arg(0x60, "lpParam")?)?;

    // Read window title if present
    let title = if window_title == 0 {
        String::new()
    } else {
        read_guest_ansi_lossy(engine, window_title, 512)
            .context("failed to read CreateWindowExA window title")?
    };

    // Convert style/ex_style to u32 once (avoids repeated `as` conversions).
    let style = u32::try_from(style_raw).context("CreateWindowExA: style does not fit u32")?;
    let ex_style = u32::try_from(ex_style).context("CreateWindowExA: ex_style does not fit u32")?;

    // Handle CW_USEDEFAULT (0x8000_0000 stored as i32 = i32::MIN on the stack).
    // A CHILD window with CW_USEDEFAULT x/y is placed at (0,0) of the parent's
    // client area; only a top-level window gets the cascaded (100,100).
    let x = if x_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        x_raw
    };
    let y = if y_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        y_raw
    };
    let width = if width_raw == i32::MIN {
        640
    } else {
        width_raw
    };
    let height = if height_raw == i32::MIN {
        480
    } else {
        height_raw
    };

    let class_identifier = read_window_class_identifier_a(engine, class_value)
        .context("failed to read window class identifier for CreateWindowExA")?;

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
        false,
    )
    .context("failed to create window record for CreateWindowExA")?;

    if hwnd == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateWindowExA")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
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
        let cs_va = state.heap_state.heap.alloc_coherent(engine, 0x50);
        if cs_va == 0 {
            // Allocation failed — return HWND without WM_CREATE
            let ra = engine
                .return_from_win64_api(hwnd)
                .context("failed to return from CreateWindowExA")?;
            return Ok(WinApiHandlerResult {
                return_address: ra,
                return_value: hwnd,
            });
        }

        // One shared-lock borrow instead of eleven per-field writes. The
        // CREATESTRUCT layout + pinned offsets (dwExStyle @0x48) live in
        // `crate::guest_layout::CreateStruct`; the zero-fill covers both
        // alignment pads.
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
        .context("failed to write CREATESTRUCT for CreateWindowExA")?;

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
    let ra = engine
        .return_from_win64_api(hwnd)
        .context("failed to return from CreateWindowExA")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: hwnd,
    })
}

/// Handles `USER32.dll!CreateWindowExW`.
pub fn handle_create_window_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Read 4 register args
    let ex_style = engine
        .read_rcx()
        .context("failed to read RCX for CreateWindowExW")?;
    let class_value = engine
        .read_rdx()
        .context("failed to read RDX for CreateWindowExW")?;
    let window_title = engine
        .read_r8()
        .context("failed to read R8 for CreateWindowExW")?;
    let style_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateWindowExW")?;

    // Read 8 stack args
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateWindowExW")?;

    let stack_arg = |offset: u64, name: &str| -> Result<u64> {
        rsp.checked_add(offset)
            .with_context(|| format!("CreateWindowExW: {name} address overflow"))
    };

    let x_raw = read_guest_i32(engine, stack_arg(0x28, "X")?)?;
    let y_raw = read_guest_i32(engine, stack_arg(0x30, "Y")?)?;
    let width_raw = read_guest_i32(engine, stack_arg(0x38, "nWidth")?)?;
    let height_raw = read_guest_i32(engine, stack_arg(0x40, "nHeight")?)?;
    let parent_handle = read_guest_u64(engine, stack_arg(0x48, "hWndParent")?)?;
    let menu_handle = read_guest_u64(engine, stack_arg(0x50, "hMenu")?)?;
    let instance_handle = read_guest_u64(engine, stack_arg(0x58, "hInstance")?)?;
    let create_params = read_guest_u64(engine, stack_arg(0x60, "lpParam")?)?;

    // Read window title if present (UTF-16 for the W variant)
    let title = if window_title == 0 {
        String::new()
    } else {
        read_guest_utf16_lossy(engine, window_title, 512)
            .context("failed to read CreateWindowExW window title")?
    };

    // Handle CW_USEDEFAULT (0x8000_0000) — stored as a 32-bit DWORD in the
    // stack slot, so it reads back as i32::MIN through read_guest_i32.
    // A CHILD window with CW_USEDEFAULT x/y is placed at (0,0) of the
    // parent's client area; only a top-level window gets the cascaded
    // (100,100).
    let x = if x_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        x_raw
    };
    let y = if y_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        y_raw
    };
    let width = if width_raw == i32::MIN {
        640
    } else {
        width_raw
    };
    let height = if height_raw == i32::MIN {
        480
    } else {
        height_raw
    };

    // Convert style/ex_style to u32 once (avoids repeated `as` conversions).
    let style = u32::try_from(style_raw).context("CreateWindowExW: style does not fit u32")?;
    let ex_style = u32::try_from(ex_style).context("CreateWindowExW: ex_style does not fit u32")?;

    let class_identifier = read_window_class_identifier_w(engine, class_value)
        .context("failed to read window class identifier for CreateWindowExW")?;

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
        true,
    )
    .context("failed to create window record for CreateWindowExW")?;

    if hwnd == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateWindowExW")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // A new top-level window registers with the host-visible window set and
    // the z-order (see the ANSI variant for the full rationale).
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
        let cs_va = state.heap_state.heap.alloc_coherent(engine, 0x50);
        if cs_va == 0 {
            let ra = engine
                .return_from_win64_api(hwnd)
                .context("failed to return from CreateWindowExW")?;
            return Ok(WinApiHandlerResult {
                return_address: ra,
                return_value: hwnd,
            });
        }

        // CREATESTRUCT layout is shared with the ANSI variant (see above).
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
        .context("failed to write CREATESTRUCT for CreateWindowExW")?;

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
    let ra = engine
        .return_from_win64_api(hwnd)
        .context("failed to return from CreateWindowExW")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: hwnd,
    })
}
