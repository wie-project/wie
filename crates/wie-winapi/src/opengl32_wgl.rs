//! WGL context + surface handlers: fake `HGLRC` handles, per-HDC pixel
//! formats, the current-context pairing, and `wglSwapBuffers` — which
//! publishes the rendered backbuffer through the present path (the same
//! surface GDI/D3D9 use). Real handle semantics; the `gl*` core lives in the
//! parent module's `render` submodule.

use super::*;

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
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglCreateContext")?;
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
    let hglrc = engine
        .read_rcx()
        .context("failed to read RCX for wglDeleteContext")?;
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
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglMakeCurrent")?;
    let hglrc = engine
        .read_rdx()
        .context("failed to read RDX for wglMakeCurrent")?;
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

/// `PROC wglGetProcAddress(LPCSTR name)` — NULL for every extension.
///
/// Extension entry points are never published; Qt-class apps fall back to
/// their non-extension paths when the pointer is NULL.
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
    let name_va = engine
        .read_rcx()
        .context("failed to read RCX for wglGetProcAddress")?;
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
pub(super) fn handle_wgl_choose_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglChoosePixelFormat")?;
    let _ppfd = engine
        .read_rdx()
        .context("failed to read RDX for wglChoosePixelFormat")?;
    ctx.finish(u64::from(STUB_PIXEL_FORMAT))
}

/// `BOOL wglSetPixelFormat(HDC hdc, int format,
/// const PIXELFORMATDESCRIPTOR *ppfd)` — records the format on the HDC.
pub(super) fn handle_wgl_set_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglSetPixelFormat")?;
    let format = low_u32(engine.read_rdx()?, "wglSetPixelFormat format")?;
    let _ppfd = engine
        .read_r8()
        .context("failed to read R8 for wglSetPixelFormat")?;
    if let Some(mut gl) = lock_gl_state() {
        gl.pixel_formats.insert(hdc, format);
    }
    ctx.finish(1)
}

/// `int wglDescribePixelFormat(HDC hdc, int iPixelFormat, UINT nBytes,
/// const PIXELFORMATDESCRIPTOR *ppfd)` — writes the descriptor and reports
/// one pixel format.
pub(super) fn handle_wgl_describe_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglDescribePixelFormat")?;
    let _format = engine
        .read_rdx()
        .context("failed to read RDX for wglDescribePixelFormat")?;
    let n_bytes = engine
        .read_r8()
        .context("failed to read R8 for wglDescribePixelFormat")?;
    let ppfd_va = engine
        .read_r9()
        .context("failed to read R9 for wglDescribePixelFormat")?;
    if ppfd_va != 0 && n_bytes >= PIXELFORMATDESCRIPTOR_SIZE {
        write_pixel_format_descriptor(engine, ppfd_va)?;
    }
    ctx.finish(u64::from(STUB_PIXEL_FORMAT))
}

/// `int wglGetPixelFormat(HDC hdc)` — the format `wglSetPixelFormat` stored,
/// or 0 when none was set.
pub(super) fn handle_wgl_get_pixel_format(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglGetPixelFormat")?;
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
pub(super) fn handle_wgl_swap_buffers(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for wglSwapBuffers")?;
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
    state.present().ensure_surface(hwnd, width, height);
    // Snapshot the context's backbuffer (0RGB) — the frame the user sees.
    let frame = with_current_gl(|gl_ctx| render::gl_frame_0rgb(gl_ctx, width, height));
    if let Some((fw, fh, pixels)) = frame
        && let Some(surface) = state.present().surfaces.get_mut(&hwnd)
    {
        if fw == width && fh == height {
            let n = surface.pixels.len().min(pixels.len());
            if let (Some(dst), Some(src)) = (surface.pixels.get_mut(..n), pixels.get(..n)) {
                dst.copy_from_slice(src);
            }
        } else {
            wie_cpu::stretch_nearest(&mut surface.pixels, &pixels, fw, fh, width, height);
        }
    }
    state.present().publish(hwnd);
    tracing::trace!(target: "wiegui", hdc, width, height, "wglSwapBuffers published a rendered frame");
    ctx.finish(1)
}

/// `BOOL wglShareLists(HGLRC hglrc1, HGLRC hglrc2)` — no-op success (texture
/// sharing across contexts is documented-missing).
pub(super) fn handle_wgl_share_lists(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hglrc1 = engine
        .read_rcx()
        .context("failed to read RCX for wglShareLists")?;
    let _hglrc2 = engine
        .read_rdx()
        .context("failed to read RDX for wglShareLists")?;
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
