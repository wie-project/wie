//! WGL context + surface handlers: fake `HGLRC` handles, per-HDC pixel
//! formats, the current-context pairing, and `wglSwapBuffers` — which
//! publishes the rendered backbuffer through the present path (the same
//! surface GDI/D3D9 use). Real handle semantics; the `gl*` core lives in the
//! parent module's `render` submodule.

use super::*;
use crate::gdi32::{ArgReg, read_arg};

/// The stub's single pixel-format index (`wglChoosePixelFormat` / describe).
const STUB_PIXEL_FORMAT: u32 = 1;
/// `PIXELFORMATDESCRIPTOR` size in bytes (wingdi.h).
const PIXELFORMATDESCRIPTOR_SIZE: u64 = 40;
/// `PFD_DRAW_TO_WINDOW` (wingdi.h).
const PFD_DRAW_TO_WINDOW: u32 = 0x0000_0004;
/// `PFD_SUPPORT_OPENGL` (wingdi.h).
const PFD_SUPPORT_OPENGL: u32 = 0x0000_0020;
/// `PFD_DOUBLEBUFFER` (wingdi.h).
const PFD_DOUBLEBUFFER: u32 = 0x0000_0001;

// ── WGL handlers ────────────────────────────────────────────────────────

/// `HGLRC wglCreateContext(HDC hdc)` — allocates a fake handle with a fresh
/// pipeline context.
pub(super) fn handle_wgl_create_context(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = read_arg(engine, ArgReg::Rcx, "wglCreateContext")?;
    // Fail closed (0) when the table is poisoned — a context cannot exist.
    let handle = lock_gl_state().map_or(0, |mut gl| {
        let handle = gl.next_handle;
        gl.next_handle = handle.wrapping_add(1);
        gl.contexts.insert(handle, render::GlCtx::new());
        handle
    });
    ctx.finish(handle)
}

/// `BOOL wglDeleteContext(HGLRC hglrc)` — removes the fake handle + pipeline.
pub(super) fn handle_wgl_delete_context(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hglrc = read_arg(engine, ArgReg::Rcx, "wglDeleteContext")?;
    if let Some(mut gl) = lock_gl_state() {
        gl.contexts.remove(&hglrc);
        // Deleting the current context releases it (real WGL semantics).
        if gl.current.is_some_and(|(_, current)| current == hglrc) {
            gl.current = None;
        }
    }
    ctx.finish(1)
}

/// `BOOL wglMakeCurrent(HDC hdc, HGLRC hglrc)` — records the pairing; a NULL
/// context releases it. Sizes the context's framebuffer to the DC's window so
/// the first frame's clears/draws have a target.
pub(super) fn handle_wgl_make_current(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = read_arg(engine, ArgReg::Rcx, "wglMakeCurrent")?;
    let hglrc = read_arg(engine, ArgReg::Rdx, "wglMakeCurrent")?;
    if let Some(mut gl) = lock_gl_state() {
        gl.current = (hglrc != 0).then_some((hdc, hglrc));
        if hglrc != 0
            && let Some(gl_ctx) = gl.contexts.get_mut(&hglrc)
            && let Some(resolved) = crate::gdi32::resolve_dest_info(state, hdc)
        {
            render::gl_ensure_framebuffer(gl_ctx, resolved.width, resolved.height);
        }
    }
    ctx.finish(1)
}

/// `PROC wglGetProcAddress(LPCSTR name)` — a callable fake VA for every
/// `opengl32.dll` export (the dispatch table is the source of truth; every
/// name is preplanted in [`crate::dynamic_apis::PREPLANTED_SOFT_APIS`] at a
/// stable soft index, so the returned address stops and dispatches exactly
/// like an IAT import). Unknown / extension names → NULL, the spec behavior
/// for names this renderer does not export — Qt falls back to its
/// non-extension paths.
pub(super) fn handle_wgl_get_proc_address(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let name_va = read_arg(engine, ArgReg::Rcx, "wglGetProcAddress")?;
    if name_va != 0 {
        let name = crate::guest_string::read_ansi_lossy(engine, name_va, 256).unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        // Fail fast against the dispatch census (the source of truth), then
        // encode by position in the preplanted soft table — the runtime
        // plants it in order at session bootstrap, so the position here is
        // the index `encode_unresolved` must carry.
        if !super::OPENGL32_EXPORT_NAMES.contains(&lower.as_str()) {
            return ctx.finish(0);
        }
        let address = crate::dynamic_apis::PREPLANTED_SOFT_APIS
            .iter()
            .position(|e| e.library.eq_ignore_ascii_case("opengl32.dll") && e.name == lower)
            .and_then(|idx| u16::try_from(idx).ok())
            .map(crate::fake_va::encode_unresolved)
            .unwrap_or(0);
        return ctx.finish(address);
    }
    ctx.finish(0)
}

/// `int wglChoosePixelFormat(HDC hdc, const PIXELFORMATDESCRIPTOR *ppfd)`
/// — the single RGBA double-buffer format always matches.
pub(crate) fn handle_wgl_choose_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = read_arg(engine, ArgReg::Rcx, "wglChoosePixelFormat")?;
    let _ppfd = read_arg(engine, ArgReg::Rdx, "wglChoosePixelFormat")?;
    ctx.finish(u64::from(STUB_PIXEL_FORMAT))
}

/// `BOOL wglSetPixelFormat(HDC hdc, int format,
/// const PIXELFORMATDESCRIPTOR *ppfd)` — records the format on the HDC.
pub(crate) fn handle_wgl_set_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hdc = read_arg(engine, ArgReg::Rcx, "wglSetPixelFormat")?;
    let format = low_u32(engine.read_rdx()?, "wglSetPixelFormat format")?;
    let _ppfd = read_arg(engine, ArgReg::R8, "wglSetPixelFormat")?;
    if let Some(mut gl) = lock_gl_state() {
        gl.pixel_formats.insert(hdc, format);
    }
    ctx.finish(1)
}

/// `int wglDescribePixelFormat(HDC hdc, int iPixelFormat, UINT nBytes,
/// const PIXELFORMATDESCRIPTOR *ppfd)` — writes the descriptor and reports
/// one pixel format.
pub(crate) fn handle_wgl_describe_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = read_arg(engine, ArgReg::Rcx, "wglDescribePixelFormat")?;
    let _format = read_arg(engine, ArgReg::Rdx, "wglDescribePixelFormat")?;
    let n_bytes = read_arg(engine, ArgReg::R8, "wglDescribePixelFormat")?;
    let ppfd_va = read_arg(engine, ArgReg::R9, "wglDescribePixelFormat")?;
    if ppfd_va != 0 && n_bytes >= PIXELFORMATDESCRIPTOR_SIZE {
        write_pixel_format_descriptor(engine, ppfd_va)?;
    }
    ctx.finish(u64::from(STUB_PIXEL_FORMAT))
}

/// `int wglGetPixelFormat(HDC hdc)` — the format `wglSetPixelFormat` stored,
/// or 0 when none was set.
pub(crate) fn handle_wgl_get_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hdc = read_arg(engine, ArgReg::Rcx, "wglGetPixelFormat")?;
    let format = lock_gl_state()
        .and_then(|gl| gl.pixel_formats.get(&hdc).copied())
        .unwrap_or(0);
    ctx.finish(u64::from(format))
}

/// `BOOL wglSwapBuffers(HDC hdc)` — publishes the rendered backbuffer
/// through the present path (the same surface GDI/D3D9 use).
///
/// The DC resolves to the top-level surface exactly like the GDI blit path
/// (`crate::gdi32::resolve_dest_info`); an unresolvable DC falls back to the
/// topmost top-level window. With no window at all the swap is a silent
/// no-op — still TRUE, so the guest's present loop keeps running.
pub(crate) fn handle_wgl_swap_buffers(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = read_arg(engine, ArgReg::Rcx, "wglSwapBuffers")?;
    let resolved = crate::gdi32::resolve_dest_info(state, hdc);
    let (hwnd, width, height) = match resolved {
        Some(dest) => (dest.hwnd, dest.width, dest.height),
        None => {
            let Some(hwnd) = state.present().z_order.last().copied() else {
                return ctx.finish(1);
            };
            let (w, h) = crate::user32::window_client_size(state, hwnd.as_u64());
            (
                hwnd,
                u32::try_from(w.max(1)).unwrap_or(1),
                u32::try_from(h.max(1)).unwrap_or(1),
            )
        }
    };
    // Q9/C: in-place pooled target — the GL backbuffer is blitted directly
    // into the pooled WindowSurface slice (the one `ensure_surface` will hand
    // back via spare-buffer pooling) with no intermediate `Vec<u32>` copy. If
    // the GL framebuffer size differs from the surface, stretch_nearest writes
    // directly into the pooled slice (no temp).
    state.present().ensure_surface(hwnd, width, height);
    // Take the pooled surface Vec out (pointer move, no copy), hand its
    // mutable slice to the GL present as render target, then restore it.
    let (logical_w, h, padded_w) = {
        let s = state
            .present()
            .surfaces
            .get(&hwnd)
            .expect("surface ensured");
        (s.width, s.height, s.stride)
    };
    let mut pooled = std::mem::take(
        &mut state
            .present()
            .surfaces
            .get_mut(&hwnd)
            .expect("surface ensured")
            .pixels,
    );
    let did_blit = with_current_gl(|gl_ctx| {
        render::gl_frame_into(gl_ctx, &mut pooled, logical_w, h, padded_w, width, height)
    })
    .unwrap_or(false);
    // Restore the pooled allocation (capacity retained) for publish.
    state
        .present()
        .surfaces
        .get_mut(&hwnd)
        .expect("surface ensured")
        .pixels = pooled;
    if did_blit {
        state.present().publish(hwnd);
    } else {
        // Fallback: no GL context — publish the (cleared) surface.
        let frame = with_current_gl(|gl_ctx| render::gl_frame_0rgb(gl_ctx, width, height));
        if let Some((fw, fh, pixels)) = frame {
            state.present().blit_frame(hwnd, &pixels, fw, fh);
        } else {
            state.present().publish(hwnd);
        }
    }
    tracing::trace!(target: "wiegui", hdc, width, height, "wglSwapBuffers published a rendered frame");
    ctx.finish(1)
}

/// `BOOL wglShareLists(HGLRC hglrc1, HGLRC hglrc2)` — no-op success (texture
/// sharing across contexts is documented-missing).
pub(super) fn handle_wgl_share_lists(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hglrc1 = read_arg(engine, ArgReg::Rcx, "wglShareLists")?;
    let _hglrc2 = read_arg(engine, ArgReg::Rdx, "wglShareLists")?;
    ctx.finish(1)
}

/// `HGLRC wglGetCurrentContext(void)` — the stored pairing's context, or 0.
pub(super) fn handle_wgl_get_current_context(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let value = lock_gl_state()
        .and_then(|gl| gl.current)
        .map_or(0, |(_, hglrc)| hglrc);
    ctx.finish(value)
}

/// `HDC wglGetCurrentDC(void)` — the stored pairing's DC, or 0.
pub(super) fn handle_wgl_get_current_dc(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let value = lock_gl_state()
        .and_then(|gl| gl.current)
        .map_or(0, |(hdc, _)| hdc);
    ctx.finish(value)
}

/// Write the `PIXELFORMATDESCRIPTOR` (40 bytes, wingdi.h layout).
fn write_pixel_format_descriptor(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<()> {
    write_u16(
        engine,
        va,
        u16::try_from(PIXELFORMATDESCRIPTOR_SIZE).unwrap_or(0),
    )?; // nSize
    write_u16(engine, va.wrapping_add(2), 1)?; // nVersion
    write_u32(
        engine,
        va.wrapping_add(4),
        PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
    )?; // dwFlags
    // Offsets 8..28 — iPixelType (PFD_TYPE_RGBA), cColorBits (32), the zeroed
    // color/accumulator fields, cDepthBits (24), then the trailing zeroed
    // stencil/aux/layer bytes.
    let fixed_bytes: [u8; 20] = [
        0,  // iPixelType
        32, // cColorBits
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,  // cRedBits..cAccumAlphaBits
        24, // cDepthBits
        0, 0, 0, 0, // cStencilBits, cAuxBuffers, iLayerType, bReserved
    ];
    engine.mem_write(va.wrapping_add(8), &fixed_bytes)?;
    write_u32(engine, va.wrapping_add(28), 0)?; // dwLayerMask
    write_u32(engine, va.wrapping_add(32), 0)?; // dwVisibleMask
    write_u32(engine, va.wrapping_add(36), 0)?; // dwDamageMask
    Ok(())
}
