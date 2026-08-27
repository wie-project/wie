//! GL 1.1 fixed-function software pipeline (host-side, no guest memory).
//!
//! This module owns the per-context pipeline state ([`GlCtx`]) and the
//! rasterizer; the ABI-facing `gl*`/wgl handlers in the parent module
//! (`opengl32.rs`) read guest registers/memory and call into here. The
//! transform math (matrix multiply, near-plane clip, viewport mapping) is
//! shared with the D3D9 software renderer (`crate::d3d9_render`): GL's
//! column-major matrices feed the same row-vector helpers unchanged (a GL
//! `M·v` column-vector product equals the helper's `v·M` over the same
//! stored array), and the depth convention is reconciled by mapping GL NDC
//! z ∈ [-1, 1] to the [0, 1] viewport depth range (near = 0), which matches
//! GL's default `GL_LESS` depth test against a 1.0 clear.

use ahash::HashMap;

pub(super) mod arrays;
mod fragment;
pub(super) mod glsl;
pub(super) mod glsl_exec;
pub(super) mod light;
pub(super) mod lists;
mod matrix;
mod raster;
mod sample;

use crate::d3d9_render::{
    IDENTITY, Mat4, NEAR_CLIP_W, ScreenVertex, Viewport, clip_to_viewport, mat4_mul,
    transform_point,
};
use arrays::{BufferObject, ClientArray, GL_ARRAY_BUFFER_BINDING, GL_ELEMENT_ARRAY_BUFFER_BINDING};
use glsl::{ProgramObject, ShaderObject};
use light::{GL_FLAT, GL_LIGHT0, GL_LIGHTING, GL_SMOOTH, LightState};
use lists::{ListCompileState, ListObject, ListOp, gl_capture};
use matrix::{frustum_matrix, ortho_matrix, rotate_matrix, scale_matrix, translate_matrix};
use sample::{sample_texel, texel_footprint};

// ── GL 1.1 constants (gl.h) ─────────────────────────────────────────────

pub(crate) const GL_MODELVIEW: u32 = 0x1700;
pub(crate) const GL_PROJECTION: u32 = 0x1701;
pub(crate) const GL_TEXTURE: u32 = 0x1702;
const GL_POINTS: u32 = 0x0000;
const GL_LINES: u32 = 0x0001;
const GL_LINE_LOOP: u32 = 0x0002;
const GL_LINE_STRIP: u32 = 0x0003;
const GL_TRIANGLES: u32 = 0x0004;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_TRIANGLE_FAN: u32 = 0x0006;
const GL_QUADS: u32 = 0x0007;
const GL_QUAD_STRIP: u32 = 0x0008;
const GL_POLYGON: u32 = 0x0009;
pub(crate) const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
pub(crate) const GL_DEPTH_BUFFER_BIT: u32 = 0x0000_0100;
const GL_DEPTH_TEST: u32 = 0x0B71;
const GL_BLEND: u32 = 0x0BE2;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_NEVER: u32 = 0x0200;
const GL_LESS: u32 = 0x0201;
const GL_EQUAL: u32 = 0x0202;
const GL_LEQUAL: u32 = 0x0203;
const GL_GREATER: u32 = 0x0204;
const GL_NOTEQUAL: u32 = 0x0205;
const GL_GEQUAL: u32 = 0x0206;
const GL_ALWAYS: u32 = 0x0207;
const GL_ZERO: u32 = 0;
const GL_ONE: u32 = 1;
const GL_SRC_COLOR: u32 = 0x0300;
const GL_ONE_MINUS_SRC_COLOR: u32 = 0x0301;
const GL_SRC_ALPHA: u32 = 0x0302;
const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
const GL_DST_ALPHA: u32 = 0x0304;
const GL_ONE_MINUS_DST_ALPHA: u32 = 0x0305;
const GL_DST_COLOR: u32 = 0x0306;
const GL_ONE_MINUS_DST_COLOR: u32 = 0x0307;
const GL_TEXTURE_ENV: u32 = 0x2300;
const GL_TEXTURE_ENV_MODE: u32 = 0x2200;
pub(crate) const GL_MODULATE: u32 = 0x2100;
pub(crate) const GL_REPLACE: u32 = 0x1E01;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_NEAREST: u32 = 0x2600;
const GL_LINEAR: u32 = 0x2601;
const GL_REPEAT: u32 = 0x2901;
const GL_CLAMP: u32 = 0x2900;
const GL_CLAMP_TO_EDGE: u32 = 0x812F;
pub(crate) const GL_RGBA: u32 = 0x1908;
pub(crate) const GL_RGB: u32 = 0x1907;
pub(crate) const GL_LUMINANCE: u32 = 0x1909;
pub(crate) const GL_ALPHA: u32 = 0x1906;
pub(crate) const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub(crate) const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_FRONT_AND_BACK: u32 = 0x0408;
const GL_POINT: u32 = 0x1B00;
const GL_LINE: u32 = 0x1B01;
const GL_FILL: u32 = 0x1B02;
pub(crate) const GL_NO_ERROR: u32 = 0;
pub(crate) const GL_INVALID_ENUM: u32 = 0x0500;
pub(crate) const GL_INVALID_VALUE: u32 = 0x0501;
pub(crate) const GL_INVALID_OPERATION: u32 = 0x0502;
/// `GL_OUT_OF_MEMORY` — part of the documented error set; the software
/// renderer never fails an allocation (Vec aborts on OOM), so no code path
/// sets it.
#[allow(dead_code)]
pub(crate) const GL_OUT_OF_MEMORY: u32 = 0x0505;
pub(crate) const GL_VIEWPORT: u32 = 0x0BA2;
pub(crate) const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
/// `GL_MAX_LIGHTS` (glGetIntegerv).
pub(crate) const GL_MAX_LIGHTS: u32 = 0x0D31;
/// `GL_MAX_LIST_NESTING`.
pub(crate) const GL_MAX_LIST_NESTING: u32 = 0x0B31;
const GL_MODELVIEW_MATRIX: u32 = 0x0BA6;
const GL_PROJECTION_MATRIX: u32 = 0x0BA7;
const GL_TEXTURE_MATRIX: u32 = 0x0BA8;
const GL_CURRENT_COLOR: u32 = 0x0B00;
const GL_CURRENT_TEXTURE_COORDS: u32 = 0x0B03;
/// Maximum matrix-stack depth (GL guarantees ≥ 32 for MODELVIEW).
const MAX_MATRIX_DEPTH: usize = 32;
/// Advertised `GL_MAX_TEXTURE_SIZE` (the sampler handles any size).
pub(crate) const MAX_TEXTURE_SIZE: u32 = 2048;
/// `GL_NEAREST_MIPMAP_*` / `GL_LINEAR_MIPMAP_*` filter names (mipmaps are
/// documented-missing; the names map to their base filter).
const MIPMAP_MIN_FILTERS: [u32; 4] = [0x2700, 0x2701, 0x2702, 0x2703];

/// One immediate-mode vertex (the current-color / current-texcoord snapshot
/// at `glVertex*` time).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlVertex {
    /// Object-space position `(x, y, z, w)`.
    pos: [f32; 4],
    /// Current RGBA color (0..1 each).
    color: [f32; 4],
    /// Current texture coordinate `(s, t, r, q)`.
    tex: [f32; 4],
    /// Current normal (lighting + vertex-shader input).
    normal: [f32; 3],
}

/// A guest texture object (`glGenTextures`/`glBindTexture`).
#[derive(Debug)]
pub(crate) struct TextureObject {
    /// GL texture name (never 0).
    id: u32,
    /// Texel width.
    width: u32,
    /// Texel height.
    height: u32,
    /// Texels in `0xAARRGGBB`, row-major, TOP row first (the GL bottom-up
    /// upload rows are flipped here so the sampler matches the backbuffer).
    pixels: Vec<u32>,
    /// `GL_NEAREST` / `GL_LINEAR` minification filter.
    min_filter: u32,
    /// `GL_NEAREST` / `GL_LINEAR` magnification filter.
    mag_filter: u32,
    /// `GL_REPEAT` / `GL_CLAMP` U wrap.
    wrap_s: u32,
    /// `GL_REPEAT` / `GL_CLAMP` V wrap.
    wrap_t: u32,
}

/// Per-HGLRC GL 1.1 pipeline state.
#[derive(Debug)]
pub(crate) struct GlCtx {
    /// Current matrix mode (`GL_MODELVIEW` / `GL_PROJECTION` / `GL_TEXTURE`).
    matrix_mode: u32,
    /// MODELVIEW stack (top = `last`).
    modelview_stack: Vec<Mat4>,
    /// PROJECTION stack (top = `last`).
    projection_stack: Vec<Mat4>,
    /// TEXTURE stack (top = `last`).
    texture_stack: Vec<Mat4>,
    /// Current color (RGBA, 0..1).
    current_color: [f32; 4],
    /// Current texture coordinate `(s, t, r, q)`.
    current_texcoord: [f32; 4],
    /// Current `glBegin` mode (`None` = not in a begin/end pair).
    begin_mode: Option<u32>,
    /// Vertices accumulated since `glBegin`.
    batch: Vec<GlVertex>,
    /// Framebuffer width (0 = not sized yet — sized at `wglMakeCurrent` /
    /// first swap).
    pub(crate) width: u32,
    /// Framebuffer height.
    pub(crate) height: u32,
    /// Backbuffer pixels in `0xAARRGGBB` (masked to 0RGB on publish).
    backbuffer: Vec<u32>,
    /// Depth buffer (0 = near, 1 = far).
    depth: Vec<f32>,
    /// `glClearColor` (RGBA, 0..1).
    clear_color: [f32; 4],
    /// `glClearDepth`.
    clear_depth: f32,
    /// Viewport in GL window coords `(x, y, w, h)` — y measured from the
    /// framebuffer BOTTOM, matching `glViewport` semantics.
    viewport: (i32, i32, i32, i32),
    /// Whether the viewport is still the GL default (full framebuffer).
    viewport_defaulted: bool,
    /// `GL_DEPTH_TEST` enabled.
    depth_test: bool,
    /// Depth write mask (`glDepthMask`).
    depth_mask: bool,
    /// Depth compare function (`GL_LESS` default).
    depth_func: u32,
    /// `GL_BLEND` enabled.
    blend: bool,
    /// Source blend factor (`GL_SRC_ALPHA` default).
    src_blend: u32,
    /// Destination blend factor (`GL_ONE_MINUS_SRC_ALPHA` default).
    dst_blend: u32,
    /// `GL_TEXTURE_2D` enabled.
    texture_2d: bool,
    /// Bound texture name (0 = none).
    bound_texture: u32,
    /// Texture environment mode (`GL_MODULATE` / `GL_REPLACE`).
    tex_env: u32,
    /// Polygon mode (`GL_FILL` / `GL_LINE` / `GL_POINT`).
    polygon_mode: u32,
    /// Texture objects keyed by name.
    textures: Vec<TextureObject>,
    /// Next `glGenTextures` name (names are never reused).
    next_texture_id: u32,
    /// `GL_UNPACK_ALIGNMENT` (1/2/4/8).
    pub(crate) unpack_alignment: u32,
    /// Sticky first error flag (`glGetError` reads and clears it).
    error: u32,
    // ── Vertex arrays + VBOs ────────────────────────────────────────────
    /// `GL_VERTEX_ARRAY` client state.
    vertex_array: ClientArray,
    /// `GL_COLOR_ARRAY` client state.
    color_array: ClientArray,
    /// `GL_TEXTURE_COORD_ARRAY` client state.
    texcoord_array: ClientArray,
    /// `GL_NORMAL_ARRAY` client state.
    normal_array: ClientArray,
    /// Buffer objects keyed by name.
    buffers: Vec<BufferObject>,
    /// The buffer bound to `GL_ARRAY_BUFFER` (0 = none).
    bound_array_buffer: u32,
    /// The buffer bound to `GL_ELEMENT_ARRAY_BUFFER` (0 = none).
    bound_element_buffer: u32,
    /// Next `glGenBuffers` name.
    next_buffer_id: u32,
    // ── Lighting (fixed-function, per-vertex) ───────────────────────────
    /// `GL_LIGHTING` enabled.
    lighting: bool,
    /// The eight lights (`GL_LIGHT0..GL_LIGHT7`).
    lights: [LightState; 8],
    /// `GL_LIGHT_MODEL_AMBIENT` (default `(0.2, 0.2, 0.2, 1)`).
    light_model_ambient: [f32; 4],
    /// `GL_AMBIENT` material.
    material_ambient: [f32; 4],
    /// `GL_DIFFUSE` material.
    material_diffuse: [f32; 4],
    /// `GL_SPECULAR` material.
    material_specular: [f32; 4],
    /// `GL_EMISSION` material.
    material_emission: [f32; 4],
    /// `GL_SHININESS` material exponent.
    material_shininess: f32,
    /// Current normal (immediate mode + lighting).
    current_normal: [f32; 3],
    /// `glShadeModel` — `GL_SMOOTH` (default) or `GL_FLAT`.
    shade_model: u32,
    // ── Display lists ───────────────────────────────────────────────────
    /// Compiled lists keyed by id.
    lists: Vec<ListObject>,
    /// An open `glNewList` capture (`None` = not compiling).
    list_compile: Option<ListCompileState>,
    /// Next `glGenLists` id.
    next_list_id: u32,
    /// Nested `glCallList` depth (bounds list-in-list recursion).
    replay_depth: u32,
    // ── Shaders / programs (GLSL ES 1.00) ───────────────────────────────
    /// Shader objects keyed by id.
    shaders: Vec<ShaderObject>,
    /// Program objects keyed by id.
    programs: Vec<ProgramObject>,
    /// Next `glCreateShader` id.
    next_shader_id: u32,
    /// Next `glCreateProgram` id.
    next_program_id: u32,
    /// The active program (0 = fixed function).
    pub(crate) program_active: u32,
    /// Cached index of the active program in `programs` (set by
    /// `glUseProgram`; `None` when fixed-function). Avoids a per-pixel
    /// linear scan of the program table in the fragment stage.
    pub(crate) active_program_idx: Option<usize>,
    /// `glActiveTexture` unit (0 = GL_TEXTURE0).
    pub(crate) active_texture_unit: u32,
    /// Per-unit bound texture name (unit 0 is the fixed-function texture).
    pub(crate) texture_unit_bindings: [u32; 2],
    /// Texture id → index into `textures`, rebuilt on mutation (bind /
    /// delete / texImage). Turns the per-pixel `texture2d` lookup into O(1).
    texture_index: HashMap<u32, usize>,
}

impl GlCtx {
    /// Fresh per-context state (GL 1.1 defaults).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            matrix_mode: GL_MODELVIEW,
            modelview_stack: vec![IDENTITY],
            projection_stack: vec![IDENTITY],
            texture_stack: vec![IDENTITY],
            current_color: [1.0, 1.0, 1.0, 1.0],
            current_texcoord: [0.0, 0.0, 0.0, 1.0],
            begin_mode: None,
            batch: Vec::new(),
            width: 0,
            height: 0,
            backbuffer: Vec::new(),
            depth: Vec::new(),
            clear_color: [0.0, 0.0, 0.0, 0.0],
            clear_depth: 1.0,
            viewport: (0, 0, 0, 0),
            viewport_defaulted: true,
            depth_test: false,
            depth_mask: true,
            depth_func: GL_LESS,
            blend: false,
            src_blend: GL_SRC_ALPHA,
            dst_blend: GL_ONE_MINUS_SRC_ALPHA,
            texture_2d: false,
            bound_texture: 0,
            tex_env: GL_MODULATE,
            polygon_mode: GL_FILL,
            textures: Vec::new(),
            next_texture_id: 1,
            unpack_alignment: 4,
            error: GL_NO_ERROR,
            vertex_array: ClientArray::default(),
            color_array: ClientArray::default(),
            texcoord_array: ClientArray::default(),
            normal_array: ClientArray::default(),
            buffers: Vec::new(),
            bound_array_buffer: 0,
            bound_element_buffer: 0,
            next_buffer_id: 1,
            lighting: false,
            lights: std::array::from_fn(|_| LightState::default()),
            light_model_ambient: [0.2, 0.2, 0.2, 1.0],
            material_ambient: [0.2, 0.2, 0.2, 1.0],
            material_diffuse: [0.8, 0.8, 0.8, 1.0],
            material_specular: [0.0, 0.0, 0.0, 1.0],
            material_emission: [0.0, 0.0, 0.0, 1.0],
            material_shininess: 0.0,
            current_normal: [0.0, 0.0, 1.0],
            shade_model: GL_SMOOTH,
            lists: Vec::new(),
            list_compile: None,
            next_list_id: 1,
            replay_depth: 0,
            shaders: Vec::new(),
            programs: Vec::new(),
            next_shader_id: 1,
            next_program_id: 1,
            program_active: 0,
            active_program_idx: None,
            active_texture_unit: 0,
            texture_unit_bindings: [0, 0],
            texture_index: HashMap::default(),
        }
    }

    /// Record `err` as the sticky error if none is pending (first error wins).
    fn set_error(&mut self, err: u32) {
        if self.error == GL_NO_ERROR {
            self.error = err;
        }
    }
}

/// Top of the current matrix stack (mutable).
fn current_matrix(ctx: &mut GlCtx) -> &mut Vec<Mat4> {
    match ctx.matrix_mode {
        GL_PROJECTION => &mut ctx.projection_stack,
        GL_TEXTURE => &mut ctx.texture_stack,
        _ => &mut ctx.modelview_stack,
    }
}

/// Top of the current matrix stack (shared).
fn current_matrix_ref(ctx: &GlCtx) -> &[Mat4] {
    match ctx.matrix_mode {
        GL_PROJECTION => &ctx.projection_stack,
        GL_TEXTURE => &ctx.texture_stack,
        _ => &ctx.modelview_stack,
    }
}
// ── Color packing ───────────────────────────────────────────────────────

/// Pack RGBA (0..1) to `0xAARRGGBB` (rounded, clamped).
#[must_use]
fn pack_color(c: [f32; 4]) -> u32 {
    let ch = |v: f32| u8::try_from((v.clamp(0.0, 1.0) * 255.0).round() as i32).unwrap_or(0);
    (u32::from(ch(c[3])) << 24)
        | (u32::from(ch(c[0])) << 16)
        | (u32::from(ch(c[1])) << 8)
        | u32::from(ch(c[2]))
}

/// Unpack `0xAARRGGBB` to RGBA (0..1).
#[must_use]
fn unpack_color(c: u32) -> [f32; 4] {
    let ch = |shift: u32| f32::from(u8::try_from((c >> shift) & 0xFF).unwrap_or(0)) / 255.0;
    [ch(16), ch(8), ch(0), ch(24)]
}
// ── Transform + primitive emission ──────────────────────────────────────

/// A clip-space vertex carrying the shader varying slots (the near-plane
/// clip interpolates every field).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlClipVertex {
    pub pos: [f32; 4],
    /// Packed color `0xAARRGGBB`.
    pub color: u32,
    /// Texture coordinate U.
    pub u: f32,
    /// Texture coordinate V.
    pub v: f32,
    /// Shader varying slots (zeros when no shader is active).
    pub varyings: [f32; glsl::MAX_VARYING_FLOATS],
}

/// Transform one vertex into clip space. With an active linked vertex
/// shader the vertex RUNS the shader (attributes from the client-array /
/// current-value path; `gl_Position` + varyings out); otherwise the
/// fixed-function modelview × projection transform applies.
fn clip_vertex(ctx: &GlCtx, v: &GlVertex) -> GlClipVertex {
    let (pos, color, u, v2, varyings) = if let Some(prog) = active_program(ctx) {
        if let Some(vs) = &prog.vs {
            let mv = ctx.modelview_stack.last().copied().unwrap_or(IDENTITY);
            let proj = ctx.projection_stack.last().copied().unwrap_or(IDENTITY);
            let exec = glsl_exec::ProgramExec {
                uniforms: &prog.uniforms,
                uniform_index: &prog.uniform_index,
            };
            let out = glsl_exec::run_vertex_shader(
                &exec,
                vs,
                glsl_exec::VsInputs {
                    pos: v.pos,
                    color: v.color,
                    normal: v.normal,
                    texcoord: v.tex,
                    mvp: mat4_mul(&mv, &proj),
                },
            );
            let color = if out.wrote_front_color {
                pack_color(out.front_color)
            } else {
                pack_color(v.color)
            };
            (out.position, color, v.tex[0], v.tex[1], out.varyings)
        } else {
            ffp_clip(ctx, v)
        }
    } else {
        ffp_clip(ctx, v)
    };
    GlClipVertex {
        pos,
        color,
        u,
        v: v2,
        varyings,
    }
}

/// The fixed-function transform (projection × modelview, texture matrix on
/// the texcoords).
fn ffp_clip(
    ctx: &GlCtx,
    v: &GlVertex,
) -> ([f32; 4], u32, f32, f32, [f32; glsl::MAX_VARYING_FLOATS]) {
    let mv = ctx.modelview_stack.last().copied().unwrap_or(IDENTITY);
    let proj = ctx.projection_stack.last().copied().unwrap_or(IDENTITY);
    let tex_m = ctx.texture_stack.last().copied().unwrap_or(IDENTITY);
    // GL applies the modelview FIRST, then the projection: `mat4_mul(a, b)`
    // composes "apply a then b" (the d3d9 row-vector convention the helpers
    // share), so the combined matrix is mv·proj, not proj·mv.
    let combined = mat4_mul(&mv, &proj);
    let pos = transform_point(v.pos, &combined);
    let tc = transform_point(v.tex, &tex_m);
    (
        pos,
        pack_color(v.color),
        tc[0],
        tc[1],
        [0.0; glsl::MAX_VARYING_FLOATS],
    )
}

/// The active program object (0 = fixed function, or a program without a
/// linked stage).
fn active_program(ctx: &GlCtx) -> Option<&glsl::ProgramObject> {
    if ctx.program_active == 0 {
        return None;
    }
    // Cached index from gl_use_program; avoids a per-pixel linear scan.
    ctx.active_program_idx
        .and_then(|idx| ctx.programs.get(idx))
        .filter(|p| p.id == ctx.program_active)
}

/// The effective viewport: the GL default (full framebuffer) until the app
/// calls `glViewport`; GL window coords (y from the bottom) are converted to
/// the top-down backbuffer rect here.
#[must_use]
fn gl_viewport_info(ctx: &GlCtx) -> Viewport {
    if ctx.viewport_defaulted || ctx.width == 0 {
        return Viewport {
            x: 0,
            y: 0,
            width: ctx.width,
            height: ctx.height,
            min_z: 0.0,
            max_z: 1.0,
        };
    }
    let (x, y, w, h) = ctx.viewport;
    let y_top = i32::try_from(ctx.height)
        .unwrap_or(0)
        .saturating_sub(y)
        .saturating_sub(h);
    Viewport {
        x: u32::try_from(x.max(0)).unwrap_or(0),
        y: u32::try_from(y_top.max(0)).unwrap_or(0),
        width: u32::try_from(w.max(0)).unwrap_or(0),
        height: u32::try_from(h.max(0)).unwrap_or(0),
        min_z: 0.0,
        max_z: 1.0,
    }
}

/// Sutherland–Hodgman clip against `w > NEAR_CLIP_W`, interpolating the
/// color/uv AND the varying slots at the crossing.
#[must_use]
fn gl_clip_polygon_near(verts: &[GlClipVertex]) -> Vec<GlClipVertex> {
    let Some(first) = verts.first().copied() else {
        return Vec::new();
    };
    let inside = |v: &GlClipVertex| v.pos[3] > NEAR_CLIP_W;
    let mut out = Vec::new();
    let mut prev = first;
    let mut prev_inside = inside(&prev);
    for &curr in verts {
        let curr_inside = inside(&curr);
        if curr_inside {
            if !prev_inside {
                out.push(near_intersect(prev, curr));
            }
            out.push(curr);
        } else if prev_inside {
            out.push(near_intersect(prev, curr));
        }
        prev = curr;
        prev_inside = curr_inside;
    }
    out
}

#[must_use]
fn near_intersect(a: GlClipVertex, b: GlClipVertex) -> GlClipVertex {
    let wa = a.pos[3];
    let wb = b.pos[3];
    let t = (wa - NEAR_CLIP_W) / (wa - wb);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let mut varyings = [0.0_f32; glsl::MAX_VARYING_FLOATS];
    for (i, slot) in varyings.iter_mut().enumerate() {
        let av = a.varyings.get(i).copied().unwrap_or(0.0);
        let bv = b.varyings.get(i).copied().unwrap_or(0.0);
        *slot = lerp(av, bv);
    }
    GlClipVertex {
        pos: [
            lerp(a.pos[0], b.pos[0]),
            lerp(a.pos[1], b.pos[1]),
            lerp(a.pos[2], b.pos[2]),
            NEAR_CLIP_W,
        ],
        color: lerp_color_packed(a.color, b.color, t),
        u: lerp(a.u, b.u),
        v: lerp(a.v, b.v),
        varyings,
    }
}

/// Packed-color lerp for the clip edge.
#[must_use]
fn lerp_color_packed(a: u32, b: u32, t: f32) -> u32 {
    let mix = |x: u8, y: u8| {
        let x = f32::from(x);
        let y = f32::from(y);
        (x + (y - x) * t).round() as u8
    };
    let ch = |v: u32, shift: u32| u8::try_from((v >> shift) & 0xFF).unwrap_or(0);
    let r = mix(ch(a, 16), ch(b, 16));
    let g = mix(ch(a, 8), ch(b, 8));
    let bl = mix(ch(a, 0), ch(b, 0));
    let al = mix(ch(a, 24), ch(b, 24));
    (u32::from(al) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(bl)
}

/// Map a clipped vertex to a screen vertex (viewport transform + varyings).
#[must_use]
fn gl_screen_from_clip(v: &GlClipVertex, vp: &Viewport) -> Option<raster::GlScreenVertex> {
    let (x, y, z, w) = clip_to_viewport(v.pos, vp)?;
    Some(raster::GlScreenVertex {
        x,
        y,
        z,
        w,
        color: v.color,
        u: v.u,
        v: v.v,
        varyings: v.varyings,
    })
}

/// Draw a clip-space primitive: a point (1 vertex), a segment (2), or a
/// polygon fan (3+) — near-clipping each against `w > EPS` before the
/// viewport mapping. The polygon mode selects fill / edges / vertices.
fn draw_clip_vertices(ctx: &mut GlCtx, verts: &[GlClipVertex]) {
    if ctx.width == 0 || ctx.height == 0 || verts.is_empty() {
        return;
    }
    let vp = gl_viewport_info(ctx);
    if vp.width == 0 || vp.height == 0 {
        return;
    }
    match verts.len() {
        1 => {
            if let Some(v) = verts.first()
                && v.pos[3] > NEAR_CLIP_W
                && let Some(s) = gl_screen_from_clip(v, &vp)
            {
                raster::rasterize_point(ctx, s);
            }
        }
        2 => {
            let clipped = gl_clip_polygon_near(verts);
            if clipped.len() == 2
                && let (Some(sa), Some(sb)) = (
                    gl_screen_from_clip(&clipped[0], &vp),
                    gl_screen_from_clip(&clipped[1], &vp),
                )
            {
                raster::rasterize_line(ctx, sa, sb);
            }
        }
        _ => {
            let clipped = gl_clip_polygon_near(verts);
            let Some(first) = clipped.first().copied() else {
                return;
            };
            match ctx.polygon_mode {
                GL_POINT => {
                    for v in &clipped {
                        if let Some(s) = gl_screen_from_clip(v, &vp) {
                            raster::rasterize_point(ctx, s);
                        }
                    }
                }
                GL_LINE => {
                    for pair in clipped.windows(2) {
                        if let (Some(sa), Some(sb)) = (
                            gl_screen_from_clip(&pair[0], &vp),
                            gl_screen_from_clip(&pair[1], &vp),
                        ) {
                            raster::rasterize_line(ctx, sa, sb);
                        }
                    }
                    if clipped.len() > 2
                        && let (Some(first_v), Some(last_v)) = (clipped.first(), clipped.last())
                        && let (Some(sa), Some(sb)) = (
                            gl_screen_from_clip(first_v, &vp),
                            gl_screen_from_clip(last_v, &vp),
                        )
                    {
                        raster::rasterize_line(ctx, sa, sb);
                    }
                }
                _ => {
                    for pair in clipped.get(1..).unwrap_or(&[]).windows(2) {
                        if let (Some(sa), Some(sb), Some(sc)) = (
                            gl_screen_from_clip(&first, &vp),
                            gl_screen_from_clip(&pair[0], &vp),
                            gl_screen_from_clip(&pair[1], &vp),
                        ) {
                            raster::rasterize_triangle(ctx, sa, sb, sc);
                        }
                    }
                }
            }
        }
    }
}

// ── Public API (called by the opengl32.rs handlers) ─────────────────────

pub(crate) fn gl_begin(ctx: &mut GlCtx, mode: u32) {
    // Display-list capture: in GL_COMPILE the op is recorded and skipped.
    if !gl_capture(ctx, ListOp::Begin(mode)) {
        return;
    }
    if ctx.begin_mode.is_some() {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    if !matches!(
        mode,
        GL_POINTS
            | GL_LINES
            | GL_LINE_LOOP
            | GL_LINE_STRIP
            | GL_TRIANGLES
            | GL_TRIANGLE_STRIP
            | GL_TRIANGLE_FAN
            | GL_QUADS
            | GL_QUAD_STRIP
            | GL_POLYGON
    ) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.begin_mode = Some(mode);
    ctx.batch.clear();
}

pub(crate) fn gl_vertex(ctx: &mut GlCtx, x: f32, y: f32, z: f32, w: f32) {
    if !gl_capture(ctx, ListOp::Vertex(x, y, z, w)) {
        return;
    }
    if ctx.begin_mode.is_none() {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    // GL lighting replaces the vertex color at vertex time (the material +
    // enabled lights, per-vertex Gouraud).
    let color = ctx.lit_color([x, y, z, w], ctx.current_normal, ctx.current_color);
    ctx.batch.push(GlVertex {
        pos: [x, y, z, w],
        color,
        tex: ctx.current_texcoord,
        normal: ctx.current_normal,
    });
}

/// `glNormal3f(x, y, z)` — the current normal (lighting input).
pub(crate) fn gl_normal(ctx: &mut GlCtx, x: f32, y: f32, z: f32) {
    if !gl_capture(ctx, ListOp::Normal(x, y, z)) {
        return;
    }
    ctx.current_normal = [x, y, z];
}

/// `glShadeModel(mode)` — `GL_SMOOTH` (per-vertex) / `GL_FLAT` (first vertex
/// color of each primitive).
pub(crate) fn gl_shade_model(ctx: &mut GlCtx, mode: u32) {
    if !gl_capture(ctx, ListOp::ShadeModel(mode)) {
        return;
    }
    match mode {
        GL_SMOOTH | GL_FLAT => ctx.shade_model = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    }
}

/// Emit a batch of transformed vertices as `mode` primitives — the shared
/// core for `glEnd` (immediate mode) and the array draws. `GL_FLAT` shades
/// the whole primitive with the first vertex's color.
pub(super) fn draw_gl_vertices(ctx: &mut GlCtx, mode: u32, verts: &[GlVertex]) {
    let clip: Vec<GlClipVertex> = if ctx.shade_model == GL_FLAT {
        let first_color = verts.first().map_or([1.0; 4], |v| v.color);
        verts
            .iter()
            .map(|v| {
                let flat = GlVertex {
                    color: first_color,
                    ..*v
                };
                clip_vertex(ctx, &flat)
            })
            .collect()
    } else {
        verts.iter().map(|v| clip_vertex(ctx, v)).collect()
    };
    match mode {
        GL_POINTS => {
            for v in &clip {
                draw_clip_vertices(ctx, std::slice::from_ref(v));
            }
        }
        GL_LINES => {
            for pair in clip.chunks_exact(2) {
                draw_clip_vertices(ctx, pair);
            }
        }
        GL_LINE_STRIP => {
            for pair in clip.windows(2) {
                draw_clip_vertices(ctx, pair);
            }
        }
        GL_LINE_LOOP => {
            for pair in clip.windows(2) {
                draw_clip_vertices(ctx, pair);
            }
            if clip.len() >= 2
                && let (Some(first), Some(last)) = (clip.first(), clip.last())
            {
                draw_clip_vertices(ctx, &[*first, *last]);
            }
        }
        GL_TRIANGLES => {
            for tri in clip.chunks_exact(3) {
                draw_clip_vertices(ctx, tri);
            }
        }
        GL_TRIANGLE_STRIP => {
            for tri in clip.windows(3) {
                draw_clip_vertices(ctx, tri);
            }
        }
        GL_TRIANGLE_FAN | GL_POLYGON => {
            for pair in clip.get(1..).unwrap_or(&[]).windows(2) {
                if let Some(first) = clip.first()
                    && let (Some(b), Some(c)) = (pair.first(), pair.get(1))
                {
                    draw_clip_vertices(ctx, &[*first, *b, *c]);
                }
            }
        }
        GL_QUADS => {
            for quad in clip.chunks_exact(4) {
                if let (Some(a), Some(b), Some(c), Some(d)) =
                    (quad.first(), quad.get(1), quad.get(2), quad.get(3))
                {
                    draw_clip_vertices(ctx, &[*a, *b, *c]);
                    draw_clip_vertices(ctx, &[*a, *c, *d]);
                }
            }
        }
        GL_QUAD_STRIP => {
            let mut i = 0_usize;
            while i.saturating_add(3) < clip.len() {
                if let (Some(a), Some(b), Some(c), Some(d)) = (
                    clip.get(i),
                    clip.get(i.saturating_add(1)),
                    clip.get(i.saturating_add(3)),
                    clip.get(i.saturating_add(2)),
                ) {
                    draw_clip_vertices(ctx, &[*a, *b, *c]);
                    draw_clip_vertices(ctx, &[*a, *c, *d]);
                }
                i = i.saturating_add(2);
            }
        }
        _ => {}
    }
}

pub(crate) fn gl_end(ctx: &mut GlCtx) {
    if !gl_capture(ctx, ListOp::End) {
        return;
    }
    let Some(mode) = ctx.begin_mode.take() else {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    };
    let verts = std::mem::take(&mut ctx.batch);
    draw_gl_vertices(ctx, mode, &verts);
}

pub(crate) fn gl_color(ctx: &mut GlCtx, r: f32, g: f32, b: f32, a: f32) {
    if !gl_capture(ctx, ListOp::Color(r, g, b, a)) {
        return;
    }
    ctx.current_color = [r, g, b, a];
}

pub(crate) fn gl_color_ub(ctx: &mut GlCtx, r: u8, g: u8, b: u8, a: u8) {
    if !gl_capture(ctx, ListOp::ColorUb(r, g, b, a)) {
        return;
    }
    let ch = |v: u8| f32::from(v) / 255.0;
    ctx.current_color = [ch(r), ch(g), ch(b), ch(a)];
}

pub(crate) fn gl_tex_coord(ctx: &mut GlCtx, s: f32, t: f32, r: f32, q: f32) {
    if !gl_capture(ctx, ListOp::TexCoord(s, t, r, q)) {
        return;
    }
    ctx.current_texcoord = [s, t, r, q];
}

pub(crate) fn gl_clear_color(ctx: &mut GlCtx, r: f32, g: f32, b: f32, a: f32) {
    ctx.clear_color = [r, g, b, a];
}

pub(crate) fn gl_clear_depth(ctx: &mut GlCtx, d: f32) {
    ctx.clear_depth = d;
}

pub(crate) fn gl_clear(ctx: &mut GlCtx, mask: u32) {
    if ctx.width == 0 || ctx.height == 0 {
        return;
    }
    if mask & GL_COLOR_BUFFER_BIT != 0 {
        let color = pack_color(ctx.clear_color);
        for pixel in &mut ctx.backbuffer {
            *pixel = color;
        }
    }
    if mask & GL_DEPTH_BUFFER_BIT != 0 {
        for slot in &mut ctx.depth {
            *slot = ctx.clear_depth;
        }
    }
}

pub(crate) fn gl_matrix_mode(ctx: &mut GlCtx, mode: u32) {
    if !gl_capture(ctx, ListOp::MatrixMode(mode)) {
        return;
    }
    match mode {
        GL_MODELVIEW | GL_PROJECTION | GL_TEXTURE => ctx.matrix_mode = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    }
}

pub(crate) fn gl_load_identity(ctx: &mut GlCtx) {
    if !gl_capture(ctx, ListOp::LoadIdentity) {
        return;
    }
    if let Some(top) = current_matrix(ctx).last_mut() {
        *top = IDENTITY;
    }
}

pub(crate) fn gl_load_matrix(ctx: &mut GlCtx, m: &Mat4) {
    if !gl_capture(ctx, ListOp::LoadMatrix(*m)) {
        return;
    }
    if let Some(top) = current_matrix(ctx).last_mut() {
        *top = *m;
    }
}

pub(crate) fn gl_mult_matrix(ctx: &mut GlCtx, m: &Mat4) {
    if !gl_capture(ctx, ListOp::MultMatrix(*m)) {
        return;
    }
    if let Some(top) = current_matrix(ctx).last_mut() {
        *top = mat4_mul(top, m);
    }
}

pub(crate) fn gl_ortho(ctx: &mut GlCtx, l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) {
    if !gl_capture(ctx, ListOp::Ortho(l, r, b, t, n, f)) {
        return;
    }
    gl_mult_matrix(ctx, &ortho_matrix(l, r, b, t, n, f));
}

pub(crate) fn gl_frustum(ctx: &mut GlCtx, l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) {
    if !gl_capture(ctx, ListOp::Frustum(l, r, b, t, n, f)) {
        return;
    }
    gl_mult_matrix(ctx, &frustum_matrix(l, r, b, t, n, f));
}

pub(crate) fn gl_translate(ctx: &mut GlCtx, x: f32, y: f32, z: f32) {
    if !gl_capture(ctx, ListOp::Translate(x, y, z)) {
        return;
    }
    gl_mult_matrix(ctx, &translate_matrix(x, y, z));
}

pub(crate) fn gl_rotate(ctx: &mut GlCtx, angle: f32, x: f32, y: f32, z: f32) {
    if !gl_capture(ctx, ListOp::Rotate(angle, x, y, z)) {
        return;
    }
    gl_mult_matrix(ctx, &rotate_matrix(angle, x, y, z));
}

pub(crate) fn gl_scale(ctx: &mut GlCtx, x: f32, y: f32, z: f32) {
    if !gl_capture(ctx, ListOp::Scale(x, y, z)) {
        return;
    }
    gl_mult_matrix(ctx, &scale_matrix(x, y, z));
}

pub(crate) fn gl_push_matrix(ctx: &mut GlCtx) {
    if !gl_capture(ctx, ListOp::PushMatrix) {
        return;
    }
    let full = current_matrix_ref(ctx).len() >= MAX_MATRIX_DEPTH;
    if full {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    if let Some(top) = current_matrix(ctx).last().copied() {
        current_matrix(ctx).push(top);
    }
}

pub(crate) fn gl_pop_matrix(ctx: &mut GlCtx) {
    if !gl_capture(ctx, ListOp::PopMatrix) {
        return;
    }
    let single = current_matrix_ref(ctx).len() <= 1;
    if single {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    current_matrix(ctx).pop();
}

pub(crate) fn gl_viewport(ctx: &mut GlCtx, x: i32, y: i32, w: i32, h: i32) {
    if !gl_capture(ctx, ListOp::Viewport(x, y, w, h)) {
        return;
    }
    if w < 0 || h < 0 {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    ctx.viewport = (x, y, w, h);
    ctx.viewport_defaulted = false;
}

pub(crate) fn gl_depth_func(ctx: &mut GlCtx, func: u32) {
    if !gl_capture(ctx, ListOp::DepthFunc(func)) {
        return;
    }
    if matches!(
        func,
        GL_NEVER
            | GL_LESS
            | GL_EQUAL
            | GL_LEQUAL
            | GL_GREATER
            | GL_NOTEQUAL
            | GL_GEQUAL
            | GL_ALWAYS
    ) {
        ctx.depth_func = func;
    } else {
        ctx.set_error(GL_INVALID_ENUM);
    }
}

pub(crate) fn gl_depth_mask(ctx: &mut GlCtx, flag: u32) {
    if !gl_capture(ctx, ListOp::DepthMask(flag)) {
        return;
    }
    ctx.depth_mask = flag != 0;
}

pub(crate) fn gl_blend_func(ctx: &mut GlCtx, src: u32, dst: u32) {
    if !gl_capture(ctx, ListOp::BlendFunc(src, dst)) {
        return;
    }
    if !is_blend_factor(src) || !is_blend_factor(dst) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.src_blend = src;
    ctx.dst_blend = dst;
}

#[must_use]
fn is_blend_factor(f: u32) -> bool {
    matches!(
        f,
        GL_ZERO
            | GL_ONE
            | GL_SRC_COLOR
            | GL_ONE_MINUS_SRC_COLOR
            | GL_SRC_ALPHA
            | GL_ONE_MINUS_SRC_ALPHA
            | GL_DST_ALPHA
            | GL_ONE_MINUS_DST_ALPHA
            | GL_DST_COLOR
            | GL_ONE_MINUS_DST_COLOR
    )
}

pub(crate) fn gl_enable(ctx: &mut GlCtx, cap: u32) {
    if !gl_capture(ctx, ListOp::Enable(cap)) {
        return;
    }
    match cap {
        GL_DEPTH_TEST => ctx.depth_test = true,
        GL_BLEND => ctx.blend = true,
        GL_TEXTURE_2D => ctx.texture_2d = true,
        GL_LIGHTING => ctx.lighting = true,
        GL_LIGHT0..=0x4007 => ctx.set_light_enabled(cap, true),
        _ => {
            // Legacy apps enable many caps this renderer does not model
            // (culling, fog, scissor, color-material, ...). Accept and ignore
            // — the lenient behavior real drivers exhibit for legacy apps.
            tracing::debug!(target: "wiegui", cap, "glEnable: accepted-and-ignored cap");
        }
    }
}

pub(crate) fn gl_disable(ctx: &mut GlCtx, cap: u32) {
    if !gl_capture(ctx, ListOp::Disable(cap)) {
        return;
    }
    match cap {
        GL_DEPTH_TEST => ctx.depth_test = false,
        GL_BLEND => ctx.blend = false,
        GL_TEXTURE_2D => ctx.texture_2d = false,
        GL_LIGHTING => ctx.lighting = false,
        GL_LIGHT0..=0x4007 => ctx.set_light_enabled(cap, false),
        _ => {
            tracing::debug!(target: "wiegui", cap, "glDisable: accepted-and-ignored cap");
        }
    }
}

pub(crate) fn gl_polygon_mode(ctx: &mut GlCtx, face: u32, mode: u32) {
    if !gl_capture(ctx, ListOp::PolygonMode(face, mode)) {
        return;
    }
    if face != GL_FRONT_AND_BACK {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    match mode {
        GL_POINT | GL_LINE | GL_FILL => ctx.polygon_mode = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    }
}

impl GlCtx {
    /// Rebuild the texture id → index map. Called on mutation (bind-create /
    /// delete); textures are few, so a full rebuild on a rare event beats a
    /// per-pixel linear scan in `texture2d`.
    fn rebuild_texture_index(&mut self) {
        self.texture_index.clear();
        for (idx, t) in self.textures.iter().enumerate() {
            self.texture_index.insert(t.id, idx);
        }
    }
}

pub(crate) fn gl_gen_textures(ctx: &mut GlCtx, count: u32) -> Vec<u32> {
    let mut names = Vec::new();
    for _ in 0..count {
        let mut name = ctx.next_texture_id;
        while name == 0 || ctx.textures.iter().any(|t| t.id == name) {
            name = name.wrapping_add(1);
        }
        ctx.next_texture_id = name.wrapping_add(1);
        names.push(name);
    }
    names
}

pub(crate) fn gl_delete_textures(ctx: &mut GlCtx, names: &[u32]) {
    for name in names {
        if *name == ctx.bound_texture {
            ctx.bound_texture = 0;
        }
        ctx.textures.retain(|t| t.id != *name);
    }
    ctx.rebuild_texture_index();
}

pub(crate) fn gl_bind_texture(ctx: &mut GlCtx, target: u32, name: u32) {
    if !gl_capture(ctx, ListOp::BindTexture(name)) {
        return;
    }
    if target != GL_TEXTURE_2D {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    if name == 0 {
        ctx.bound_texture = 0;
        if let Some(slot) = ctx
            .texture_unit_bindings
            .get_mut(usize::try_from(ctx.active_texture_unit).unwrap_or(usize::MAX))
        {
            *slot = 0;
        }
        return;
    }
    // Binding a name with no object creates the texture (GL semantics).
    if !ctx.textures.iter().any(|t| t.id == name) {
        ctx.textures.push(TextureObject {
            id: name,
            width: 0,
            height: 0,
            pixels: Vec::new(),
            min_filter: GL_NEAREST,
            mag_filter: GL_NEAREST,
            wrap_s: GL_REPEAT,
            wrap_t: GL_REPEAT,
        });
        ctx.rebuild_texture_index();
    }
    // The binding targets the ACTIVE texture unit; unit 0 is also the
    // fixed-function texture.
    if let Some(slot) = ctx
        .texture_unit_bindings
        .get_mut(usize::try_from(ctx.active_texture_unit).unwrap_or(usize::MAX))
    {
        *slot = name;
    }
    if ctx.active_texture_unit == 0 {
        ctx.bound_texture = name;
    }
}

pub(crate) fn gl_tex_parameter_i(ctx: &mut GlCtx, target: u32, pname: u32, param: u32) {
    if target != GL_TEXTURE_2D {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    let Some(tex) = ctx.textures.iter_mut().find(|t| t.id == ctx.bound_texture) else {
        return; // no bound texture: GL ignores the call
    };
    match pname {
        GL_TEXTURE_MIN_FILTER => {
            let base = base_min_filter(param);
            if base == 0 {
                ctx.set_error(GL_INVALID_ENUM);
            } else {
                tex.min_filter = base;
            }
        }
        GL_TEXTURE_MAG_FILTER => {
            if matches!(param, GL_NEAREST | GL_LINEAR) {
                tex.mag_filter = param;
            } else {
                ctx.set_error(GL_INVALID_ENUM);
            }
        }
        GL_TEXTURE_WRAP_S | GL_TEXTURE_WRAP_T => {
            let clamp_like = matches!(param, GL_CLAMP | GL_CLAMP_TO_EDGE);
            if !clamp_like && param != GL_REPEAT {
                ctx.set_error(GL_INVALID_ENUM);
                return;
            }
            let resolved = if clamp_like { GL_CLAMP } else { GL_REPEAT };
            if pname == GL_TEXTURE_WRAP_S {
                tex.wrap_s = resolved;
            } else {
                tex.wrap_t = resolved;
            }
        }
        _ => {
            // Unmodeled texture parameters (borders, mip base/range, ...)
            // are accepted and ignored, matching lenient legacy drivers.
            tracing::debug!(target: "wiegui", pname, "glTexParameteri: accepted-and-ignored pname");
        }
    }
}

/// The base minification filter for a `GL_TEXTURE_MIN_FILTER` value; 0 =
/// invalid. Mipmap-named filters map to their base (mipmaps documented-missing).
#[must_use]
fn base_min_filter(param: u32) -> u32 {
    if matches!(param, GL_NEAREST | GL_LINEAR) {
        param
    } else if MIPMAP_MIN_FILTERS.contains(&param) {
        if param == 0x2700 || param == 0x2702 {
            GL_NEAREST
        } else {
            GL_LINEAR
        }
    } else {
        0
    }
}

/// `glTexImage2D` — uploads texel bytes (GL bottom-up row order) into the
/// bound texture. `pixels: None` is a NULL upload (texture undefined).
#[allow(clippy::too_many_arguments)]
pub(crate) fn gl_tex_image_2d(
    ctx: &mut GlCtx,
    target: u32,
    _level: u32,
    internal_format: u32,
    width: u32,
    height: u32,
    border: u32,
    format: u32,
    pixel_type: u32,
    pixels: Option<&[u8]>,
) {
    if target != GL_TEXTURE_2D {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    let bytes_per_pixel = match format {
        GL_RGBA => 4,
        GL_RGB => 3,
        GL_LUMINANCE | GL_ALPHA => 1,
        _ => {
            ctx.set_error(GL_INVALID_ENUM);
            return;
        }
    };
    if !matches!(internal_format, GL_RGBA | GL_RGB | GL_LUMINANCE | GL_ALPHA) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    if pixel_type != GL_UNSIGNED_BYTE {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    if border != 0 || width == 0 || height == 0 {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    let row_bytes = usize::try_from(bytes_per_pixel)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(width).unwrap_or(0));
    let alignment = usize::try_from(ctx.unpack_alignment).unwrap_or(4).max(1);
    let row_stride = row_bytes.div_ceil(alignment).saturating_mul(alignment);
    let needed = row_stride.saturating_mul(usize::try_from(height).unwrap_or(0));
    if let Some(bytes) = pixels
        && bytes.len() < needed
    {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    let mut texels = Vec::with_capacity(needed);
    // GL uploads row 0 = the BOTTOM row; store top-down so the sampler and
    // the backbuffer agree.
    for stored_row in 0..height {
        let guest_row = height.wrapping_sub(1).wrapping_sub(stored_row);
        let row_start = usize::try_from(guest_row)
            .unwrap_or(0)
            .saturating_mul(row_stride);
        for col in 0..width {
            let off = row_start.saturating_add(
                usize::try_from(col)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(bytes_per_pixel).unwrap_or(0)),
            );
            let get = |i: usize| pixels.and_then(|b| b.get(i)).copied().unwrap_or(0);
            let (r, g, b, a) = match format {
                GL_RGBA => (
                    get(off),
                    get(off.saturating_add(1)),
                    get(off.saturating_add(2)),
                    get(off.saturating_add(3)),
                ),
                GL_RGB => (
                    get(off),
                    get(off.saturating_add(1)),
                    get(off.saturating_add(2)),
                    255,
                ),
                GL_LUMINANCE => {
                    let l = get(off);
                    (l, l, l, 255)
                }
                _ => (0, 0, 0, get(off)),
            };
            texels.push(
                (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
            );
        }
    }
    let Some(tex) = ctx.textures.iter_mut().find(|t| t.id == ctx.bound_texture) else {
        return; // no bound texture — GL ignores the upload
    };
    tex.width = width;
    tex.height = height;
    tex.pixels = texels;
}

pub(crate) fn gl_tex_env_i(ctx: &mut GlCtx, target: u32, pname: u32, param: u32) {
    if !gl_capture(ctx, ListOp::TexEnv(param)) {
        return;
    }
    if target != GL_TEXTURE_ENV || pname != GL_TEXTURE_ENV_MODE {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    match param {
        GL_MODULATE | GL_REPLACE => ctx.tex_env = param,
        _ => ctx.set_error(GL_INVALID_ENUM),
    }
}

pub(crate) fn gl_pixel_store_i(ctx: &mut GlCtx, pname: u32, param: u32) {
    if pname == GL_UNPACK_ALIGNMENT {
        if matches!(param, 1 | 2 | 4 | 8) {
            ctx.unpack_alignment = param;
        } else {
            ctx.set_error(GL_INVALID_VALUE);
        }
    } else {
        tracing::debug!(target: "wiegui", pname, "glPixelStorei: accepted-and-ignored pname");
    }
}

pub(crate) fn gl_get_error(ctx: &mut GlCtx) -> u32 {
    let err = ctx.error;
    ctx.error = GL_NO_ERROR;
    err
}

/// Values for `glGetIntegerv` (empty = the caller writes one 0).
pub(crate) fn gl_integerv(ctx: &mut GlCtx, pname: u32) -> Vec<u32> {
    match pname {
        GL_VIEWPORT => {
            let (x, y, w, h) = ctx.viewport;
            vec![
                u32::try_from(x.max(0)).unwrap_or(0),
                u32::try_from(y.max(0)).unwrap_or(0),
                u32::try_from(w.max(0)).unwrap_or(0),
                u32::try_from(h.max(0)).unwrap_or(0),
            ]
        }
        GL_MAX_TEXTURE_SIZE => vec![MAX_TEXTURE_SIZE],
        GL_UNPACK_ALIGNMENT => vec![ctx.unpack_alignment],
        GL_ARRAY_BUFFER_BINDING | GL_ELEMENT_ARRAY_BUFFER_BINDING => {
            let (array_binding, element_binding) = arrays::buffer_bindings(ctx);
            if pname == GL_ARRAY_BUFFER_BINDING {
                vec![array_binding]
            } else {
                vec![element_binding]
            }
        }
        GL_MAX_LIGHTS => vec![8],
        GL_MAX_LIST_NESTING => vec![64],
        _ => Vec::new(),
    }
}

/// Values for `glGetFloatv` (empty = the caller writes nothing).
pub(crate) fn gl_floatv(ctx: &mut GlCtx, pname: u32) -> Vec<f32> {
    match pname {
        GL_MODELVIEW_MATRIX => ctx
            .modelview_stack
            .last()
            .copied()
            .unwrap_or(IDENTITY)
            .to_vec(),
        GL_PROJECTION_MATRIX => ctx
            .projection_stack
            .last()
            .copied()
            .unwrap_or(IDENTITY)
            .to_vec(),
        GL_TEXTURE_MATRIX => ctx
            .texture_stack
            .last()
            .copied()
            .unwrap_or(IDENTITY)
            .to_vec(),
        GL_CURRENT_COLOR => ctx.current_color.to_vec(),
        GL_CURRENT_TEXTURE_COORDS => ctx.current_texcoord.to_vec(),
        _ => Vec::new(),
    }
}

/// `glReadPixels` — GL_RGBA / GL_UNSIGNED_BYTE reads of the backbuffer
/// (GL y from the bottom; rows outside the framebuffer read as zero).
/// Unsupported formats return an empty buffer (the caller writes nothing).
pub(crate) fn gl_read_pixels(
    ctx: &GlCtx,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    format: u32,
    pixel_type: u32,
) -> Vec<u8> {
    if format != GL_RGBA || pixel_type != GL_UNSIGNED_BYTE || width <= 0 || height <= 0 {
        return Vec::new();
    }
    let fb_w = i32::try_from(ctx.width).unwrap_or(0);
    let fb_h = i32::try_from(ctx.height).unwrap_or(0);
    let mut out = Vec::with_capacity(
        usize::try_from(width.saturating_mul(height))
            .unwrap_or(0)
            .saturating_mul(4),
    );
    for row in 0..height {
        // GL y is from the bottom; the backbuffer is top-down.
        let src_row = fb_h.saturating_sub(y.saturating_add(row)).saturating_sub(1);
        for col in 0..width {
            let sx = x.saturating_add(col);
            let pixel = if sx >= 0 && sx < fb_w && src_row >= 0 && src_row < fb_h {
                let index = usize::try_from(src_row)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(ctx.width).unwrap_or(0))
                    .saturating_add(usize::try_from(sx).unwrap_or(0));
                ctx.backbuffer.get(index).copied().unwrap_or(0)
            } else {
                0
            };
            out.push(u8::try_from((pixel >> 16) & 0xFF).unwrap_or(0));
            out.push(u8::try_from((pixel >> 8) & 0xFF).unwrap_or(0));
            out.push(u8::try_from(pixel & 0xFF).unwrap_or(0));
            out.push(u8::try_from((pixel >> 24) & 0xFF).unwrap_or(0));
        }
    }
    out
}

/// Size the context's framebuffer (backbuffer + depth) to `width` × `height`,
/// (re)initializing the depth to the clear depth. A resize resets the
/// content — the GL behavior for a default-framebuffer size change.
pub(crate) fn gl_ensure_framebuffer(ctx: &mut GlCtx, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    if ctx.width == width && ctx.height == height {
        return;
    }
    ctx.width = width;
    ctx.height = height;
    let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
    ctx.backbuffer = vec![0; needed];
    ctx.depth = vec![ctx.clear_depth; needed];
}

/// Snapshot the rendered backbuffer as a 0RGB frame for the present path,
/// (re)sizing the framebuffer to the surface when needed.
pub(crate) fn gl_frame_0rgb(
    ctx: &mut GlCtx,
    surface_w: u32,
    surface_h: u32,
) -> (u32, u32, Vec<u32>) {
    gl_ensure_framebuffer(ctx, surface_w, surface_h);
    let mut frame = Vec::with_capacity(ctx.backbuffer.len());
    for pixel in &ctx.backbuffer {
        frame.push(pixel & 0x00FF_FFFF);
    }
    (ctx.width, ctx.height, frame)
}

/// Q9/C: raster directly into the pooled WindowSurface slice (the one
/// `PresentState::ensure_surface` will hand back via spare-buffer pooling) — no intermediate
/// `Vec<u32>` copy. If the GL framebuffer size differs from the surface,
/// stretch_nearest writes directly into the pooled slice (no temp).
pub(crate) fn gl_frame_into(
    ctx: &mut GlCtx,
    dst: &mut [u32],
    logical_w: u32,
    height: u32,
    padded_w: u32,
    surface_w: u32,
    surface_h: u32,
) -> bool {
    if logical_w == 0 || height == 0 || padded_w == 0 {
        return false;
    }
    gl_ensure_framebuffer(ctx, surface_w, surface_h);
    let fw = ctx.width;
    let fh = ctx.height;
    if fw == 0 || fh == 0 {
        return false;
    }
    if fw == logical_w && fh == height {
        // Direct row-major copy with padded stride, no alloc.
        let src_stride = fw as usize;
        let dst_stride = padded_w as usize;
        for y in 0..height as usize {
            let src_start = y * src_stride;
            let dst_start = y * dst_stride;
            if src_start + logical_w as usize <= ctx.backbuffer.len()
                && dst_start + logical_w as usize <= dst.len()
            {
                for x in 0..logical_w as usize {
                    dst[dst_start + x] = ctx.backbuffer[src_start + x] & 0x00FF_FFFF;
                }
            }
        }
    } else {
        // Nearest-neighbour stretch directly into pooled slice (no temp Vec).
        let logical_w_us = logical_w as usize;
        let padded_w_us = padded_w as usize;
        let h_us = height as usize;
        let fw_us = fw as usize;
        let fh_us = fh as usize;
        if logical_w_us > 0 && h_us > 0 && fw_us > 0 && fh_us > 0 {
            for y in 0..h_us {
                let src_y = (y * fh_us) / h_us;
                let dst_row = y * padded_w_us;
                let src_row = src_y * fw_us;
                for x in 0..logical_w_us {
                    let src_x = (x * fw_us) / logical_w_us;
                    let src_idx = src_row + src_x;
                    let dst_idx = dst_row + x;
                    if src_idx < ctx.backbuffer.len() && dst_idx < dst.len() {
                        dst[dst_idx] = ctx.backbuffer[src_idx] & 0x00FF_FFFF;
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::float_cmp,
    clippy::cast_precision_loss
)]
mod tests;
