//! Handles `opengl32.dll` — WGL context/surface + a REAL GL 1.1
//! fixed-function software renderer: matrix stacks, immediate-mode vertices,
//! scanline rasterization with depth + alpha blend, and GL_RGBA textures,
//! implemented host-side in the `render` submodule. `wglSwapBuffers` copies
//! the rendered backbuffer into the present surface (the same `PresentState`
//! path GDI and D3D9 use), so legacy immediate-mode apps actually draw.
//!
//! Still stubbed (see `docs/missing-winapi-handlers.md`): VBOs, GLSL,
//! GL_LIGHTING, display lists, stencil, framebuffer objects, multisample.
//! Unknown exports are NOT silently succeeded — the dispatcher returns
//! `Ok(None)` so the caller reports an unsupported API loudly.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{Context, Result};

use crate::gdi32::resolve_dest_info;
use crate::guest_memory::{write_u16, write_u32};
use crate::kernel32::low_u32;
use crate::state::WinApiState;
use crate::{HandlerContext, WinApiHandlerResult};

/// Fake `HGLRC` handle base — clear of every other handle namespace.
const HGLRC_HANDLE_BASE: u64 = 0x0000_0000_6A00_0000;
/// Size of the `glGetString` static guest buffer (allocated on first use).
const GL_STRING_BUF_SIZE: u64 = 64;
/// `GL_VENDOR` (gl.h).
const GL_VENDOR: u64 = 0x1F00;
/// `GL_RENDERER` (gl.h).
const GL_RENDERER: u64 = 0x1F01;
/// `GL_VERSION` (gl.h).
const GL_VERSION: u64 = 0x1F02;

/// The GL 1.1 software pipeline (matrix stacks, rasterizer, textures).
#[path = "opengl32_render/mod.rs"]
mod render;

/// The WGL context/surface handlers (fake-HGLRC handle table).
#[path = "opengl32_wgl.rs"]
pub(crate) mod wgl;

/// The stage-2 `gl*` handlers (arrays, VBOs, lists, lighting).
#[path = "opengl32_gl2.rs"]
mod gl2;

/// The GLSL shader/program handlers.
#[path = "opengl32_glsl_handlers.rs"]
mod glsl_handlers;

/// Host side of the WGL/GL state: fake `HGLRC` handles (each owning a
/// [`render::GlCtx`] pipeline), per-HDC pixel formats, and the
/// current-context pairing.
///
/// Module-global (`LazyLock`) — the same shared-mutable-state seam as
/// `comctl32::IMAGE_LISTS`: `WindowState` cannot grow (its fields are owned
/// by the state module), and the GL state needs no per-session lifetime.
#[derive(Debug)]
struct GlState {
    /// Live fake `HGLRC` handles, each with its own pipeline state.
    contexts: HashMap<u64, render::GlCtx>,
    /// Monotonic fake-`HGLRC` allocator.
    next_handle: u64,
    /// Pixel format stored per HDC by `wglSetPixelFormat`.
    pixel_formats: HashMap<u64, u32>,
    /// The current `(hdc, hglrc)` pairing set by `wglMakeCurrent`.
    ///
    /// Global, not per-thread: all handlers run under the one `WinApiState`
    /// mutex, so no two guest threads can observe a pairing mid-flight.
    current: Option<(u64, u64)>,
    /// Guest VA of the `glGetString` static buffer (allocated on first use).
    gl_string_buf: u64,
}

impl Default for GlState {
    fn default() -> Self {
        Self {
            contexts: HashMap::new(),
            next_handle: HGLRC_HANDLE_BASE,
            pixel_formats: HashMap::new(),
            current: None,
            gl_string_buf: 0,
        }
    }
}

static GL_STATE: LazyLock<Mutex<GlState>> = LazyLock::new(|| Mutex::new(GlState::default()));

/// Lock the GL table, failing closed (`None`) on a poisoned lock so a
/// panicking thread cannot unwind into the guest.
fn lock_gl_state() -> Option<MutexGuard<'static, GlState>> {
    GL_STATE.lock().ok()
}

/// Resize the current GL context's backbuffer to its window's client size.
///
/// Real Windows resizes the default framebuffer with the window *before* the
/// next WM_PAINT. Called from the guest thread at `glViewport` (the start of
/// every frame) so the backbuffer is sized before any drawing — a host-side
/// resize would race with a paint in flight and reallocate the backbuffer
/// between draw and readback (zeroed reads, gl_quad exit 122).
pub fn resize_current_context(state: &mut WinApiState) {
    let Some(mut gl) = lock_gl_state() else {
        return;
    };
    let Some((hdc, hglrc)) = gl.current else {
        return;
    };
    let Some(resolved) = resolve_dest_info(state, hdc) else {
        return;
    };
    if let Some(gl_ctx) = gl.contexts.get_mut(&hglrc) {
        render::gl_ensure_framebuffer(gl_ctx, resolved.width, resolved.height);
    }
}

/// Run `f` on the current context's pipeline state; with no current context
/// the GL call is a silent no-op (WGL semantics).
fn with_current_gl<T>(f: impl FnOnce(&mut render::GlCtx) -> T) -> Option<T> {
    lock_gl_state().and_then(|mut gl| {
        let hglrc = gl.current.map_or(0, |(_, hglrc)| hglrc);
        let gl_ctx = gl.contexts.get_mut(&hglrc)?;
        Some(f(gl_ctx))
    })
}

/// Read one `f32` argument from an XMM register slot (Win64: the first four
/// float args ride XMM0..XMM3).
fn read_xmm_f32(engine: &mut dyn wie_cpu::CpuEngine, slot: usize) -> Result<f32> {
    let snapshot = engine.snapshot_thread_context();
    let raw = snapshot.xmm.get(slot).copied().unwrap_or(0);
    let bits = u32::try_from(raw & u128::from(u32::MAX)).unwrap_or(0);
    Ok(f32::from_bits(bits))
}

/// Read one `GLdouble` argument (an XMM slot) as `f32` — the `GLdouble`
/// Win64 ABI passes 64-bit floats in XMM registers; the low 32 bits of a
/// double are NOT the float value, so the double is reinterpreted whole.
fn read_xmm_double_f32(engine: &mut dyn wie_cpu::CpuEngine, slot: usize) -> Result<f32> {
    let snapshot = engine.snapshot_thread_context();
    let raw = snapshot.xmm.get(slot).copied().unwrap_or(0);
    let bits = u64::try_from(raw & u128::from(u64::MAX)).unwrap_or(0);
    Ok(f64::from_bits(bits) as f32)
}

/// Read one `GLdouble` from a stack argument slot (the 5th+ float args of a
/// 6-arg GL call, e.g. `glOrtho`'s near/far — stack doubles).
fn read_stack_double_f32(
    engine: &mut dyn wie_cpu::CpuEngine,
    offset: u64,
    name: &str,
) -> Result<f32> {
    let raw = crate::d3d9::read_stack_argument(engine, offset, name)?;
    Ok(f64::from_bits(raw) as f32)
}

/// Reinterpret the low 32 bits of a register as a signed `i32` (GLint).
#[must_use]
pub(crate) fn low_i32(raw: u64) -> i32 {
    i32::from_le_bytes(
        u32::try_from(raw & u64::from(u32::MAX))
            .unwrap_or(0)
            .to_le_bytes(),
    )
}

/// Read a little-endian `f32` from a byte slice at `offset`.
#[must_use]
fn read_f32_from(bytes: &[u8], offset: usize) -> f32 {
    let end = offset.saturating_add(4);
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    f32::from_le_bytes(raw)
}

/// Read a 64-byte column-major matrix from guest memory (no-op on failure).
fn read_guest_mat4(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
) -> Option<crate::d3d9_render::Mat4> {
    let mut bytes = [0_u8; 64];
    engine.mem_read(va, &mut bytes).ok()?;
    let mut m = [0.0_f32; 16];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = read_f32_from(&bytes, i.saturating_mul(4));
    }
    Some(m)
}

/// Handler signature shared by every `opengl32.dll` export.
type OpenglHandler = fn(&mut HandlerContext<'_>) -> Result<WinApiHandlerResult>;

/// Every implemented `opengl32.dll` export, mapped to its handler — the
/// single census + dispatch table. The macro also emits
/// [`OPENGL32_EXPORT_NAMES`] from the same list, which `is_export` and the
/// preplanted soft table (`wglGetProcAddress`) consult, so the two can never
/// drift. A name present here is by construction both dispatched and
/// reported by [`is_export`]; the linear scan is fine on this cold
/// string-dispatch path (the dense `WinApiId` table owns the hot APIs).
macro_rules! opengl32_exports {
    ($(($name:literal, $handler:path),)*) => {
        const OPENGL32_DISPATCH: &[(&str, OpenglHandler)] = &[
            $(($name, $handler),)*
        ];

        /// The export names — the census for `is_export` and the preplanted
        /// soft-table source for `wglGetProcAddress`.
        pub(crate) const OPENGL32_EXPORT_NAMES: &[&str] = &[
            $($name,)*
        ];
    };
}

opengl32_exports! {
    // ── WGL — real handle semantics ─────────────────────────────────────
    ("wglcreatecontext", wgl::handle_wgl_create_context),
    ("wgldeletecontext", wgl::handle_wgl_delete_context),
    ("wglmakecurrent", wgl::handle_wgl_make_current),
    ("wglgetprocaddress", wgl::handle_wgl_get_proc_address),
    ("wglchoosepixelformat", wgl::handle_wgl_choose_pixel_format),
    ("wglsetpixelformat", wgl::handle_wgl_set_pixel_format),
    ("wgldescribepixelformat", wgl::handle_wgl_describe_pixel_format),
    ("wglgetpixelformat", wgl::handle_wgl_get_pixel_format),
    ("wglswapbuffers", wgl::handle_wgl_swap_buffers),
    ("wglsharelists", wgl::handle_wgl_share_lists),
    ("wglgetcurrentcontext", wgl::handle_wgl_get_current_context),
    ("wglgetcurrentdc", wgl::handle_wgl_get_current_dc),
    // ── gl* core — real fixed-function pipeline ─────────────────────────
    ("glclearcolor", handle_gl_clear_color),
    ("glcleardepth", handle_gl_clear_depth),
    ("glclear", handle_gl_clear),
    ("glbegin", handle_gl_begin),
    ("glend", handle_gl_end),
    ("glvertex2f", handle_gl_vertex2f),
    ("glvertex3f", handle_gl_vertex3f),
    ("glvertex4f", handle_gl_vertex4f),
    ("glvertex2fv", handle_gl_vertex2fv),
    ("glvertex3fv", handle_gl_vertex3fv),
    ("glvertex4fv", handle_gl_vertex4fv),
    ("glcolor3f", handle_gl_color3f),
    ("glcolor4f", handle_gl_color4f),
    ("glcolor3fv", handle_gl_color3fv),
    ("glcolor4fv", handle_gl_color4fv),
    ("glcolor3ub", handle_gl_color3ub),
    ("glcolor4ub", handle_gl_color4ub),
    ("glcolor3ubv", handle_gl_color3ubv),
    ("glcolor4ubv", handle_gl_color4ubv),
    ("gltexcoord1f", handle_gl_texcoord1f),
    ("gltexcoord2f", handle_gl_texcoord2f),
    ("gltexcoord3f", handle_gl_texcoord3f),
    ("gltexcoord4f", handle_gl_texcoord4f),
    ("gltexcoord1fv", handle_gl_texcoord1fv),
    ("gltexcoord2fv", handle_gl_texcoord2fv),
    ("gltexcoord3fv", handle_gl_texcoord3fv),
    ("gltexcoord4fv", handle_gl_texcoord4fv),
    ("glmatrixmode", handle_gl_matrix_mode),
    ("glloadidentity", handle_gl_load_identity),
    ("glloadmatrixf", handle_gl_load_matrixf),
    ("glmultmatrixf", handle_gl_mult_matrixf),
    ("glortho", handle_gl_ortho),
    ("glfrustum", handle_gl_frustum),
    ("gltranslatef", handle_gl_translatef),
    ("glrotatef", handle_gl_rotatef),
    ("glscalef", handle_gl_scalef),
    ("glpushmatrix", handle_gl_push_matrix),
    ("glpopmatrix", handle_gl_pop_matrix),
    ("glviewport", handle_gl_viewport),
    ("gldepthfunc", handle_gl_depth_func),
    ("gldepthmask", handle_gl_depth_mask),
    ("glenable", handle_gl_enable),
    ("gldisable", handle_gl_disable),
    ("glblendfunc", handle_gl_blend_func),
    ("glpolygonmode", handle_gl_polygon_mode),
    ("glgentextures", handle_gl_gen_textures),
    ("gldeletetextures", handle_gl_delete_textures),
    ("glbindtexture", handle_gl_bind_texture),
    ("glteximage2d", handle_gl_tex_image_2d),
    ("gltexparameteri", handle_gl_tex_parameter_i),
    ("gltexenvi", handle_gl_tex_env_i),
    ("gltexenvf", handle_gl_tex_env_f),
    ("glpixelstorei", handle_gl_pixel_store_i),
    ("glreadpixels", handle_gl_read_pixels),
    ("glflush", handle_gl_noop),
    ("glfinish", handle_gl_noop),
    ("glgeterror", handle_gl_get_error),
    ("glgetstring", handle_gl_get_string),
    ("glgetintegerv", handle_gl_get_integerv),
    ("glgetfloatv", handle_gl_get_floatv),
    ("glgetbooleanv", handle_gl_get_booleanv),
    // ── gl* accepted no-ops — documented-missing GL features ────────────
    ("glscissor", handle_gl_noop),
    ("glenableclientstate", gl2::handle_gl_enable_client_state),
    ("gldisableclientstate", gl2::handle_gl_disable_client_state),
    ("glvertexpointer", gl2::handle_gl_vertex_pointer),
    ("glcolorpointer", gl2::handle_gl_color_pointer),
    ("gltexcoordpointer", gl2::handle_gl_texcoord_pointer),
    ("glnormalpointer", gl2::handle_gl_normal_pointer),
    ("gldrawarrays", gl2::handle_gl_draw_arrays),
    ("gldrawelements", gl2::handle_gl_draw_elements),    ("glgenbuffers", gl2::handle_gl_gen_buffers),
    ("gldeletebuffers", gl2::handle_gl_delete_buffers),
    ("glisbuffer", gl2::handle_gl_is_buffer),
    ("glbindbuffer", gl2::handle_gl_bind_buffer),
    ("glbufferdata", gl2::handle_gl_buffer_data),
    ("glbuffersubdata", gl2::handle_gl_buffer_sub_data),
    ("glreadbuffer", handle_gl_noop),
    ("gllightfv", gl2::handle_gl_light_fv),
    ("gllightmodelfv", gl2::handle_gl_light_model_fv),
    ("glmaterialfv", gl2::handle_gl_material_fv),
    ("glmaterialf", gl2::handle_gl_material_f),
    ("glcolormaterial", handle_gl_noop),
    ("glshademodel", gl2::handle_gl_shade_model),    ("glnormal3f", gl2::handle_gl_normal3f),
    ("glnormal3fv", gl2::handle_gl_normal3fv),
    ("glcullface", handle_gl_noop),
    ("glfrontface", handle_gl_noop),
    ("glpointsizef", handle_gl_noop),
    ("gllinewidth", handle_gl_noop),
    ("glnewlist", gl2::handle_gl_new_list),
    ("glendlist", gl2::handle_gl_end_list),
    ("glcalllist", gl2::handle_gl_call_list),
    ("glcalllists", gl2::handle_gl_call_lists),
    ("glgenlists", gl2::handle_gl_gen_lists),
    ("glislist", gl2::handle_gl_is_list),
    ("gldeletelists", gl2::handle_gl_delete_lists),
    ("gllistbase", handle_gl_noop),
    // ── gl* shader-object no-ops (GLSL documented-missing) ──────────────
    ("glisshader", glsl_handlers::handle_gl_is_shader),
    ("glshadersource", glsl_handlers::handle_gl_shader_source),
    ("glcompileshader", glsl_handlers::handle_gl_compile_shader),
    ("glcreateprogram", glsl_handlers::handle_gl_create_program),
    ("glcreateshader", glsl_handlers::handle_gl_create_shader),    ("gldeleteshader", glsl_handlers::handle_gl_delete_shader),
    ("glgetshaderiv", glsl_handlers::handle_gl_get_shader_iv),
    ("glgetshaderinfolog", glsl_handlers::handle_gl_get_shader_info_log),
    ("glattachshader", glsl_handlers::handle_gl_attach_shader),
    ("gldetachshader", glsl_handlers::handle_gl_detach_shader),
    ("gllinkprogram", glsl_handlers::handle_gl_link_program),
    ("gluseprogram", glsl_handlers::handle_gl_use_program),
    ("glgetprogramiv", glsl_handlers::handle_gl_get_program_iv),
    ("glgetprograminfolog", glsl_handlers::handle_gl_get_program_info_log),
    ("gldeleteprogram", glsl_handlers::handle_gl_delete_program),
    ("glisprogram", glsl_handlers::handle_gl_is_program),
    ("glvalidateprogram", glsl_handlers::handle_gl_validate_program),
    ("glgetuniformlocation", glsl_handlers::handle_gl_get_uniform_location),
    ("gluniform1f", glsl_handlers::handle_gl_uniform1f),
    ("gluniform2f", glsl_handlers::handle_gl_uniform2f),
    ("gluniform3f", glsl_handlers::handle_gl_uniform3f),
    ("gluniform4f", glsl_handlers::handle_gl_uniform4f),
    ("gluniform1i", glsl_handlers::handle_gl_uniform1i),
    ("gluniformmatrix4fv", glsl_handlers::handle_gl_uniform_matrix4fv),
    ("glactivetexture", glsl_handlers::handle_gl_active_texture),
}

/// Cold-path string dispatch for `opengl32.dll` exports (case-insensitive).
pub fn dispatch_opengl32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    let Some((_, handler)) = OPENGL32_DISPATCH.iter().find(|(key, _)| *key == n.as_str()) else {
        // Unknown export — bail loudly instead of faking success.
        return Ok(None);
    };
    Ok(Some(handler(ctx)?))
}

/// Census oracle: which `opengl32.dll` exports are implemented.
///
/// Reads the single [`OPENGL32_DISPATCH`] table, so it can never drift from
/// the dispatch match.
pub fn is_export(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    OPENGL32_DISPATCH.iter().any(|(key, _)| *key == n.as_str())
}

// The WGL handlers live in the `wgl` submodule.
// ── gl* handlers — clear / matrices / immediate mode ────────────────────

fn handle_gl_clear_color(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let r = read_xmm_f32(engine, 0).context("failed to read XMM0 for glClearColor")?;
    let g = read_xmm_f32(engine, 1).context("failed to read XMM1 for glClearColor")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glClearColor")?;
    let a = read_xmm_f32(engine, 3).context("failed to read XMM3 for glClearColor")?;
    with_current_gl(|c| render::gl_clear_color(c, r, g, b, a));
    ctx.finish(0)
}

fn handle_gl_clear_depth(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let d = read_xmm_double_f32(engine, 0).context("failed to read XMM0 for glClearDepth")?;
    with_current_gl(|c| render::gl_clear_depth(c, d));
    ctx.finish(0)
}

fn handle_gl_clear(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mask = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glClear")?,
        "glClear mask",
    )?;
    with_current_gl(|c| render::gl_clear(c, mask));
    ctx.finish(0)
}

fn handle_gl_begin(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mode = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glBegin")?,
        "glBegin mode",
    )?;
    with_current_gl(|c| render::gl_begin(c, mode));
    ctx.finish(0)
}

fn handle_gl_end(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    with_current_gl(render::gl_end);
    ctx.finish(0)
}

fn handle_gl_vertex2f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glVertex2f")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glVertex2f")?;
    with_current_gl(|c| render::gl_vertex(c, x, y, 0.0, 1.0));
    ctx.finish(0)
}

fn handle_gl_vertex3f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glVertex3f")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glVertex3f")?;
    let z = read_xmm_f32(engine, 2).context("failed to read XMM2 for glVertex3f")?;
    with_current_gl(|c| render::gl_vertex(c, x, y, z, 1.0));
    ctx.finish(0)
}

fn handle_gl_vertex4f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glVertex4f")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glVertex4f")?;
    let z = read_xmm_f32(engine, 2).context("failed to read XMM2 for glVertex4f")?;
    let w = read_xmm_f32(engine, 3).context("failed to read XMM3 for glVertex4f")?;
    with_current_gl(|c| render::gl_vertex(c, x, y, z, w));
    ctx.finish(0)
}

/// Shared body for the `glVertex*fv` forms: read 2/3/4 floats at `va`.
fn vertex_fv(
    ctx: &mut HandlerContext<'_>,
    count: usize,
    name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {name}"))?;
    let mut bytes = [0_u8; 16];
    if engine.mem_read(va, &mut bytes).is_ok() {
        let x = read_f32_from(&bytes, 0);
        let y = read_f32_from(&bytes, 4);
        let z = read_f32_from(&bytes, 8);
        let w = read_f32_from(&bytes, 12);
        let (vx, vy, vz, vw) = match count {
            2 => (x, y, 0.0, 1.0),
            3 => (x, y, z, 1.0),
            _ => (x, y, z, w),
        };
        with_current_gl(|c| render::gl_vertex(c, vx, vy, vz, vw));
    }
    ctx.finish(0)
}

fn handle_gl_vertex2fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    vertex_fv(ctx, 2, "glVertex2fv")
}
fn handle_gl_vertex3fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    vertex_fv(ctx, 3, "glVertex3fv")
}
fn handle_gl_vertex4fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    vertex_fv(ctx, 4, "glVertex4fv")
}

fn handle_gl_color3f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let r = read_xmm_f32(engine, 0).context("failed to read XMM0 for glColor3f")?;
    let g = read_xmm_f32(engine, 1).context("failed to read XMM1 for glColor3f")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glColor3f")?;
    with_current_gl(|c| render::gl_color(c, r, g, b, 1.0));
    ctx.finish(0)
}

fn handle_gl_color4f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let r = read_xmm_f32(engine, 0).context("failed to read XMM0 for glColor4f")?;
    let g = read_xmm_f32(engine, 1).context("failed to read XMM1 for glColor4f")?;
    let b = read_xmm_f32(engine, 2).context("failed to read XMM2 for glColor4f")?;
    let a = read_xmm_f32(engine, 3).context("failed to read XMM3 for glColor4f")?;
    with_current_gl(|c| render::gl_color(c, r, g, b, a));
    ctx.finish(0)
}

/// Shared body for the `glColor*fv` forms (3 or 4 floats at `va`).
fn color_fv(ctx: &mut HandlerContext<'_>, count: usize, name: &str) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {name}"))?;
    let mut bytes = [0_u8; 16];
    if engine.mem_read(va, &mut bytes).is_ok() {
        let r = read_f32_from(&bytes, 0);
        let g = read_f32_from(&bytes, 4);
        let b = read_f32_from(&bytes, 8);
        let a = if count == 4 {
            read_f32_from(&bytes, 12)
        } else {
            1.0
        };
        with_current_gl(|c| render::gl_color(c, r, g, b, a));
    }
    ctx.finish(0)
}

fn handle_gl_color3fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    color_fv(ctx, 3, "glColor3fv")
}
fn handle_gl_color4fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    color_fv(ctx, 4, "glColor4fv")
}

fn handle_gl_color3ub(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let r = u8::try_from(
        engine
            .read_rcx()
            .context("failed to read RCX for glColor3ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    let g = u8::try_from(
        engine
            .read_rdx()
            .context("failed to read RDX for glColor3ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    let b = u8::try_from(
        engine
            .read_r8()
            .context("failed to read R8 for glColor3ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    with_current_gl(|c| render::gl_color_ub(c, r, g, b, 255));
    ctx.finish(0)
}

fn handle_gl_color4ub(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let r = u8::try_from(
        engine
            .read_rcx()
            .context("failed to read RCX for glColor4ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    let g = u8::try_from(
        engine
            .read_rdx()
            .context("failed to read RDX for glColor4ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    let b = u8::try_from(
        engine
            .read_r8()
            .context("failed to read R8 for glColor4ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    let a = u8::try_from(
        engine
            .read_r9()
            .context("failed to read R9 for glColor4ub")?
            & 0xFF,
    )
    .unwrap_or(0);
    with_current_gl(|c| render::gl_color_ub(c, r, g, b, a));
    ctx.finish(0)
}

/// Shared body for the `glColor*ubv` forms (3 or 4 bytes at `va`).
fn color_ubv(
    ctx: &mut HandlerContext<'_>,
    count: usize,
    name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {name}"))?;
    let mut bytes = [0_u8; 4];
    if engine.mem_read(va, &mut bytes).is_ok() {
        let r = bytes.first().copied().unwrap_or(0);
        let g = bytes.get(1).copied().unwrap_or(0);
        let b = bytes.get(2).copied().unwrap_or(0);
        let a = if count == 4 {
            bytes.get(3).copied().unwrap_or(0)
        } else {
            255
        };
        with_current_gl(|c| render::gl_color_ub(c, r, g, b, a));
    }
    ctx.finish(0)
}

fn handle_gl_color3ubv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    color_ubv(ctx, 3, "glColor3ubv")
}
fn handle_gl_color4ubv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    color_ubv(ctx, 4, "glColor4ubv")
}

fn handle_gl_texcoord1f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = read_xmm_f32(engine, 0).context("failed to read XMM0 for glTexCoord1f")?;
    with_current_gl(|c| render::gl_tex_coord(c, s, 0.0, 0.0, 1.0));
    ctx.finish(0)
}

fn handle_gl_texcoord2f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = read_xmm_f32(engine, 0).context("failed to read XMM0 for glTexCoord2f")?;
    let t = read_xmm_f32(engine, 1).context("failed to read XMM1 for glTexCoord2f")?;
    with_current_gl(|c| render::gl_tex_coord(c, s, t, 0.0, 1.0));
    ctx.finish(0)
}

fn handle_gl_texcoord3f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = read_xmm_f32(engine, 0).context("failed to read XMM0 for glTexCoord3f")?;
    let t = read_xmm_f32(engine, 1).context("failed to read XMM1 for glTexCoord3f")?;
    let r = read_xmm_f32(engine, 2).context("failed to read XMM2 for glTexCoord3f")?;
    with_current_gl(|c| render::gl_tex_coord(c, s, t, r, 1.0));
    ctx.finish(0)
}

fn handle_gl_texcoord4f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = read_xmm_f32(engine, 0).context("failed to read XMM0 for glTexCoord4f")?;
    let t = read_xmm_f32(engine, 1).context("failed to read XMM1 for glTexCoord4f")?;
    let r = read_xmm_f32(engine, 2).context("failed to read XMM2 for glTexCoord4f")?;
    let q = read_xmm_f32(engine, 3).context("failed to read XMM3 for glTexCoord4f")?;
    with_current_gl(|c| render::gl_tex_coord(c, s, t, r, q));
    ctx.finish(0)
}

/// Shared body for the `glTexCoord*fv` forms (1/2/3/4 floats at `va`).
fn texcoord_fv(
    ctx: &mut HandlerContext<'_>,
    count: usize,
    name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {name}"))?;
    let mut bytes = [0_u8; 16];
    if engine.mem_read(va, &mut bytes).is_ok() {
        let s = read_f32_from(&bytes, 0);
        let t = read_f32_from(&bytes, 4);
        let r = read_f32_from(&bytes, 8);
        let q = read_f32_from(&bytes, 12);
        let (vs, vt, vr, vq) = match count {
            1 => (s, 0.0, 0.0, 1.0),
            2 => (s, t, 0.0, 1.0),
            3 => (s, t, r, 1.0),
            _ => (s, t, r, q),
        };
        with_current_gl(|c| render::gl_tex_coord(c, vs, vt, vr, vq));
    }
    ctx.finish(0)
}

fn handle_gl_texcoord1fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    texcoord_fv(ctx, 1, "glTexCoord1fv")
}
fn handle_gl_texcoord2fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    texcoord_fv(ctx, 2, "glTexCoord2fv")
}
fn handle_gl_texcoord3fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    texcoord_fv(ctx, 3, "glTexCoord3fv")
}
fn handle_gl_texcoord4fv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    texcoord_fv(ctx, 4, "glTexCoord4fv")
}

fn handle_gl_matrix_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let mode = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glMatrixMode")?,
        "glMatrixMode mode",
    )?;
    with_current_gl(|c| render::gl_matrix_mode(c, mode));
    ctx.finish(0)
}

fn handle_gl_load_identity(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    with_current_gl(render::gl_load_identity);
    ctx.finish(0)
}

fn handle_gl_load_matrixf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .context("failed to read RCX for glLoadMatrixf")?;
    if let Some(m) = read_guest_mat4(engine, va) {
        with_current_gl(|c| render::gl_load_matrix(c, &m));
    }
    ctx.finish(0)
}

fn handle_gl_mult_matrixf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let va = engine
        .read_rcx()
        .context("failed to read RCX for glMultMatrixf")?;
    if let Some(m) = read_guest_mat4(engine, va) {
        with_current_gl(|c| render::gl_mult_matrix(c, &m));
    }
    ctx.finish(0)
}

/// `void glOrtho(GLdouble l, GLdouble r, GLdouble b, GLdouble t,
/// GLdouble n, GLdouble f)` — all six args are 64-bit doubles (XMM0..XMM3
/// then the stack), read whole and narrowed to `f32`.
fn handle_gl_ortho(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let l = read_xmm_double_f32(engine, 0).context("failed to read XMM0 for glOrtho")?;
    let r = read_xmm_double_f32(engine, 1).context("failed to read XMM1 for glOrtho")?;
    let b = read_xmm_double_f32(engine, 2).context("failed to read XMM2 for glOrtho")?;
    let t = read_xmm_double_f32(engine, 3).context("failed to read XMM3 for glOrtho")?;
    let n = read_stack_double_f32(engine, 0x28, "glOrtho near")?;
    let f = read_stack_double_f32(engine, 0x30, "glOrtho far")?;
    with_current_gl(|c| render::gl_ortho(c, l, r, b, t, n, f));
    ctx.finish(0)
}

/// `void glFrustum(GLdouble l, GLdouble r, GLdouble b, GLdouble t,
/// GLdouble n, GLdouble f)`.
fn handle_gl_frustum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let l = read_xmm_double_f32(engine, 0).context("failed to read XMM0 for glFrustum")?;
    let r = read_xmm_double_f32(engine, 1).context("failed to read XMM1 for glFrustum")?;
    let b = read_xmm_double_f32(engine, 2).context("failed to read XMM2 for glFrustum")?;
    let t = read_xmm_double_f32(engine, 3).context("failed to read XMM3 for glFrustum")?;
    let n = read_stack_double_f32(engine, 0x28, "glFrustum near")?;
    let f = read_stack_double_f32(engine, 0x30, "glFrustum far")?;
    with_current_gl(|c| render::gl_frustum(c, l, r, b, t, n, f));
    ctx.finish(0)
}

fn handle_gl_translatef(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glTranslatef")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glTranslatef")?;
    let z = read_xmm_f32(engine, 2).context("failed to read XMM2 for glTranslatef")?;
    with_current_gl(|c| render::gl_translate(c, x, y, z));
    ctx.finish(0)
}

fn handle_gl_rotatef(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let angle = read_xmm_f32(engine, 0).context("failed to read XMM0 for glRotatef")?;
    let x = read_xmm_f32(engine, 1).context("failed to read XMM1 for glRotatef")?;
    let y = read_xmm_f32(engine, 2).context("failed to read XMM2 for glRotatef")?;
    let z = read_xmm_f32(engine, 3).context("failed to read XMM3 for glRotatef")?;
    with_current_gl(|c| render::gl_rotate(c, angle, x, y, z));
    ctx.finish(0)
}

fn handle_gl_scalef(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = read_xmm_f32(engine, 0).context("failed to read XMM0 for glScalef")?;
    let y = read_xmm_f32(engine, 1).context("failed to read XMM1 for glScalef")?;
    let z = read_xmm_f32(engine, 2).context("failed to read XMM2 for glScalef")?;
    with_current_gl(|c| render::gl_scale(c, x, y, z));
    ctx.finish(0)
}

fn handle_gl_push_matrix(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    with_current_gl(render::gl_push_matrix);
    ctx.finish(0)
}

fn handle_gl_pop_matrix(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    with_current_gl(render::gl_pop_matrix);
    ctx.finish(0)
}

fn handle_gl_viewport(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = low_i32(
        engine
            .read_rcx()
            .context("failed to read RCX for glViewport")?,
    );
    let y = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for glViewport")?,
    );
    let w = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for glViewport")?,
    );
    let h = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for glViewport")?,
    );
    with_current_gl(|c| render::gl_viewport(c, x, y, w, h));
    // The default framebuffer follows the window: glViewport is called at the
    // start of every frame with the client size, so size the backbuffer here
    // (guest thread, before drawing) instead of from the host's resize path.
    resize_current_context(ctx.state);
    ctx.finish(0)
}

fn handle_gl_depth_func(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let func = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glDepthFunc")?,
        "glDepthFunc func",
    )?;
    with_current_gl(|c| render::gl_depth_func(c, func));
    ctx.finish(0)
}

fn handle_gl_depth_mask(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let flag = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glDepthMask")?,
        "glDepthMask flag",
    )?;
    with_current_gl(|c| render::gl_depth_mask(c, flag));
    ctx.finish(0)
}

fn handle_gl_enable(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cap = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glEnable")?,
        "glEnable cap",
    )?;
    with_current_gl(|c| render::gl_enable(c, cap));
    ctx.finish(0)
}

fn handle_gl_disable(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cap = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glDisable")?,
        "glDisable cap",
    )?;
    with_current_gl(|c| render::gl_disable(c, cap));
    ctx.finish(0)
}

fn handle_gl_blend_func(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glBlendFunc")?,
        "glBlendFunc src",
    )?;
    let dst = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glBlendFunc")?,
        "glBlendFunc dst",
    )?;
    with_current_gl(|c| render::gl_blend_func(c, src, dst));
    ctx.finish(0)
}

fn handle_gl_polygon_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let face = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glPolygonMode")?,
        "glPolygonMode face",
    )?;
    let mode = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glPolygonMode")?,
        "glPolygonMode mode",
    )?;
    with_current_gl(|c| render::gl_polygon_mode(c, face, mode));
    ctx.finish(0)
}

// ── gl* handlers — textures ─────────────────────────────────────────────

fn handle_gl_gen_textures(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let count = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glGenTextures")?,
        "glGenTextures count",
    )?;
    let names_va = engine
        .read_rdx()
        .context("failed to read RDX for glGenTextures")?;
    let names = with_current_gl(|c| render::gl_gen_textures(c, count)).unwrap_or_default();
    if names_va != 0 {
        for (i, name) in names.iter().enumerate() {
            let offset = u64::try_from(i.saturating_mul(4)).unwrap_or(0);
            write_u32(engine, names_va.wrapping_add(offset), *name)?;
        }
    }
    ctx.finish(0)
}

fn handle_gl_delete_textures(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let count = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glDeleteTextures")?,
        "glDeleteTextures count",
    )?;
    let names_va = engine
        .read_rdx()
        .context("failed to read RDX for glDeleteTextures")?;
    if names_va != 0 {
        let mut names = Vec::new();
        let mut bytes = vec![0_u8; usize::try_from(count.saturating_mul(4)).unwrap_or(0)];
        if engine.mem_read(names_va, &mut bytes).is_ok() {
            for chunk in bytes.chunks(4) {
                let raw: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
                names.push(u32::from_le_bytes(raw));
            }
        }
        with_current_gl(|c| render::gl_delete_textures(c, &names));
    }
    ctx.finish(0)
}

fn handle_gl_bind_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glBindTexture")?,
        "glBindTexture target",
    )?;
    let name = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glBindTexture")?,
        "glBindTexture name",
    )?;
    with_current_gl(|c| render::gl_bind_texture(c, target, name));
    ctx.finish(0)
}

/// `void glTexImage2D(GLenum target, GLint level, GLint internalformat,
/// GLsizei width, GLsizei height, GLint border, GLenum format, GLenum type,
/// const void *pixels)`.
fn handle_gl_tex_image_2d(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glTexImage2D")?,
        "glTexImage2D target",
    )?;
    let level = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glTexImage2D")?,
        "glTexImage2D level",
    )?;
    let internal_format = low_u32(
        engine
            .read_r8()
            .context("failed to read R8 for glTexImage2D")?,
        "glTexImage2D internalformat",
    )?;
    let width = low_u32(
        engine
            .read_r9()
            .context("failed to read R9 for glTexImage2D")?,
        "glTexImage2D width",
    )?;
    let height = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x28, "glTexImage2D height")?,
        "glTexImage2D height",
    )?;
    let border = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x30, "glTexImage2D border")?,
        "glTexImage2D border",
    )?;
    let format = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x38, "glTexImage2D format")?,
        "glTexImage2D format",
    )?;
    let pixel_type = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x40, "glTexImage2D type")?,
        "glTexImage2D type",
    )?;
    let pixels_va = crate::d3d9::read_stack_argument(engine, 0x48, "glTexImage2D pixels")?;
    // The byte span depends on the format + unpack alignment; peek the
    // alignment, read the span, then upload.
    let alignment = with_current_gl(|c| c.unpack_alignment).unwrap_or(4);
    let bytes_per_pixel = match format {
        render::GL_RGBA => 4_u32,
        render::GL_RGB => 3_u32,
        render::GL_LUMINANCE | render::GL_ALPHA => 1_u32,
        _ => 0_u32,
    };
    let (width, height, bytes_per_pixel, alignment) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
        usize::try_from(bytes_per_pixel).unwrap_or(0),
        usize::try_from(alignment).unwrap_or(4).max(1),
    );
    let row_bytes = bytes_per_pixel.saturating_mul(width);
    let row_stride = row_bytes.div_ceil(alignment).saturating_mul(alignment);
    let needed = row_stride.saturating_mul(height);
    let mut bytes = vec![0_u8; needed];
    let readable = pixels_va != 0 && engine.mem_read(pixels_va, &mut bytes).is_ok();
    with_current_gl(|c| {
        render::gl_tex_image_2d(
            c,
            target,
            level,
            internal_format,
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
            border,
            format,
            pixel_type,
            if readable {
                Some(bytes.as_slice())
            } else {
                None
            },
        )
    });
    ctx.finish(0)
}

fn handle_gl_tex_parameter_i(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glTexParameteri")?,
        "glTexParameteri target",
    )?;
    let pname = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glTexParameteri")?,
        "glTexParameteri pname",
    )?;
    let param = low_u32(
        engine
            .read_r8()
            .context("failed to read R8 for glTexParameteri")?,
        "glTexParameteri param",
    )?;
    with_current_gl(|c| render::gl_tex_parameter_i(c, target, pname, param));
    ctx.finish(0)
}

fn handle_gl_tex_env_i(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glTexEnvi")?,
        "glTexEnvi target",
    )?;
    let pname = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glTexEnvi")?,
        "glTexEnvi pname",
    )?;
    let param = low_u32(
        engine
            .read_r8()
            .context("failed to read R8 for glTexEnvi")?,
        "glTexEnvi param",
    )?;
    with_current_gl(|c| render::gl_tex_env_i(c, target, pname, param));
    ctx.finish(0)
}

fn handle_gl_tex_env_f(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let target = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glTexEnvf")?,
        "glTexEnvf target",
    )?;
    let pname = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glTexEnvf")?,
        "glTexEnvf pname",
    )?;
    let param = read_xmm_f32(engine, 2).context("failed to read XMM2 for glTexEnvf")?;
    with_current_gl(|c| render::gl_tex_env_i(c, target, pname, param as u32));
    ctx.finish(0)
}

fn handle_gl_pixel_store_i(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pname = low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for glPixelStorei")?,
        "glPixelStorei pname",
    )?;
    let param = low_u32(
        engine
            .read_rdx()
            .context("failed to read RDX for glPixelStorei")?,
        "glPixelStorei param",
    )?;
    with_current_gl(|c| render::gl_pixel_store_i(c, pname, param));
    ctx.finish(0)
}

// ── gl* handlers — queries ──────────────────────────────────────────────

/// `void glReadPixels(GLint x, GLint y, GLsizei width, GLsizei height,
/// GLenum format, GLenum type, void *pixels)`.
fn handle_gl_read_pixels(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let x = low_i32(
        engine
            .read_rcx()
            .context("failed to read RCX for glReadPixels")?,
    );
    let y = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for glReadPixels")?,
    );
    let width = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for glReadPixels")?,
    );
    let height = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for glReadPixels")?,
    );
    let format = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x28, "glReadPixels format")?,
        "glReadPixels format",
    )?;
    let pixel_type = low_u32(
        crate::d3d9::read_stack_argument(engine, 0x30, "glReadPixels type")?,
        "glReadPixels type",
    )?;
    let pixels_va = crate::d3d9::read_stack_argument(engine, 0x38, "glReadPixels pixels")?;
    if pixels_va != 0 {
        let bytes =
            with_current_gl(|c| render::gl_read_pixels(c, x, y, width, height, format, pixel_type))
                .unwrap_or_default();
        if !bytes.is_empty() {
            engine.mem_write(pixels_va, &bytes)?;
        }
    }
    ctx.finish(0)
}

/// Shared void-success for the accepted-and-ignored `gl*` set (VBOs, shaders,
/// lighting, display lists, ...): every call returns, drawing nothing.
fn handle_gl_noop(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

fn handle_gl_get_error(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let err = with_current_gl(render::gl_get_error).unwrap_or(render::GL_NO_ERROR);
    ctx.finish(u64::from(err))
}

/// `const GLubyte *glGetString(GLenum name)` — a NUL-terminated ASCII string
/// from a module-owned guest-heap buffer (allocated on first use, never
/// freed). The version now honestly reports "1.1".
///
/// Unknown enum values (including `GL_EXTENSIONS`) return the empty string —
/// no extensions are claimed.
fn handle_gl_get_string(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name = engine
        .read_rcx()
        .context("failed to read RCX for glGetString")?;
    let text: &[u8] = match name {
        GL_VENDOR => b"WIE",
        GL_RENDERER => b"WIE Software Rasterizer",
        GL_VERSION => b"1.1",
        _ => b"",
    };
    let mut va = lock_gl_state().map_or(0, |gl| gl.gl_string_buf);
    if va == 0 {
        let allocated = state
            .heap_state
            .heap
            .alloc_coherent(engine, GL_STRING_BUF_SIZE);
        if allocated == 0 {
            return ctx.finish(0);
        }
        if let Some(mut gl) = lock_gl_state() {
            gl.gl_string_buf = allocated;
        }
        va = allocated;
    }
    let mut bytes = Vec::with_capacity(text.len().saturating_add(1));
    bytes.extend_from_slice(text);
    bytes.push(0);
    engine.mem_write(va, &bytes)?;
    ctx.finish(va)
}

/// `void glGetIntegerv(GLenum pname, GLint *params)` — GL_VIEWPORT (4
/// values), GL_MAX_TEXTURE_SIZE, GL_UNPACK_ALIGNMENT; anything else reads as
/// one zero (the scalar-query fallback).
fn handle_gl_get_integerv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pname = engine
        .read_rcx()
        .context("failed to read RCX for glGetIntegerv")?;
    let params_va = engine
        .read_rdx()
        .context("failed to read RDX for glGetIntegerv")?;
    if params_va != 0 {
        let pname_u32 = low_u32(pname, "glGetIntegerv pname").unwrap_or(u32::MAX);
        let values = with_current_gl(|c| render::gl_integerv(c, pname_u32)).unwrap_or_default();
        if values.is_empty() {
            write_u32(engine, params_va, 0)?;
        } else {
            for (i, value) in values.iter().enumerate() {
                let offset = u64::try_from(i.saturating_mul(4)).unwrap_or(0);
                write_u32(engine, params_va.wrapping_add(offset), *value)?;
            }
        }
    }
    ctx.finish(0)
}

/// `void glGetFloatv(GLenum pname, GLfloat *params)` — the current matrices
/// and current color/texcoord; unknown pnames write nothing.
fn handle_gl_get_floatv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pname = engine
        .read_rcx()
        .context("failed to read RCX for glGetFloatv")?;
    let params_va = engine
        .read_rdx()
        .context("failed to read RDX for glGetFloatv")?;
    if params_va != 0 {
        let pname_u32 = low_u32(pname, "glGetFloatv pname").unwrap_or(u32::MAX);
        let values = with_current_gl(|c| render::gl_floatv(c, pname_u32)).unwrap_or_default();
        for (i, value) in values.iter().enumerate() {
            let offset = u64::try_from(i.saturating_mul(4)).unwrap_or(0);
            engine.mem_write(params_va.wrapping_add(offset), &value.to_le_bytes())?;
        }
    }
    ctx.finish(0)
}

fn handle_gl_get_booleanv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _pname = engine
        .read_rcx()
        .context("failed to read RCX for glGetBooleanv")?;
    let params_va = engine
        .read_rdx()
        .context("failed to read RDX for glGetBooleanv")?;
    if params_va != 0 {
        engine.mem_write(params_va, &[0_u8])?;
    }
    ctx.finish(0)
}
