//! P3: D3D9 software-render core — FVF layout parsing, vertex transform,
//! and solid-fill triangle rasterization.
//!
//! Slice 1 scope (roadmap B6 slice 1): `D3DFVF_XYZ` / `D3DFVF_XYZRHW`
//! positions, optional `D3DFVF_NORMAL` / `D3DFVF_DIFFUSE` / `D3DFVF_SPECULAR`,
//! flat-vertex diffuse colors, a fixed-function world × view × projection
//! transform, near-plane rejection, and a top-left-rule fill with barycentric
//! color interpolation. No textures, lighting, or z-buffer (draw order).
//! The PS 2.0 interpreter runs in the fragment stage.
//!
//! Submodules: [`vertex`] (FVF layout + transform), [`sample`] (texture-stage
//! sampling + FFP color ops), [`blend`] (typed render-state enums, depth test,
//! alpha blend), [`ps`] (PS 2.0 interpreter), [`vs`] (VS 2.0 interpreter).

mod blend;
mod flow;
mod operand;
mod ps;
mod sample;
mod vertex;
mod vs;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::float_cmp,
    clippy::cast_precision_loss
)]
mod tests;

use crate::gdi32::IRect;
use wie_cpu::{blend_0rgb_4x, fill_0rgb_4x};

pub use self::blend::{
    D3dBlend, D3dBlendOp, D3dCmpFunc, D3dZBufferType, alpha_test_pass, fog_blend, fog_factor,
    is_top_or_left_edge,
};
pub use self::ps::{
    PsFragmentInput, PsProgram, pixel_shader_alpha_to_u8, pixel_shader_color_to_0rgb,
    run_pixel_shader,
};
pub use self::sample::{FragmentState, MAX_MIP_LEVELS, MipChain, MipLevelView, TextureStage};
pub use self::vertex::{
    ClipVertex, FvfLayout, GuestVertex, IDENTITY, Mat4, NEAR_CLIP_W, ScreenVertex, Viewport,
    clip_polygon_near, clip_to_screen, clip_to_viewport, mat4_mul, parse_fvf, parse_vertex,
    screen_from_clip, transform_point,
};
pub use self::vs::{VsOutput, VsProgram, VsVertexInput, run_vertex_shader, vs_input_from_vertex};

// Private cross-module helpers: rasterize_triangle (below) drives the
// fragment stage through these, so each lives in the submodule that owns it.
use self::blend::{blend_colors, blend_fragment, depth_test, edge_inside};
use self::ps::color_to_float4;
use self::sample::{eval_alpha_op, eval_color_op, sample_texture_mip, stage_arg};

/// `D3DFVF_XYZ`: untransformed position (3 floats).
pub const D3DFVF_XYZ: u32 = 0x0002;
/// `D3DFVF_XYZRHW`: pre-transformed position (4 floats, screen space).
pub const D3DFVF_XYZRHW: u32 = 0x0004;
/// `D3DFVF_NORMAL`: vertex normal (3 floats; unused in slice 1).
pub const D3DFVF_NORMAL: u32 = 0x0010;
/// `D3DFVF_DIFFUSE`: vertex color (4 bytes `0xAARRGGBB`).
pub const D3DFVF_DIFFUSE: u32 = 0x0040;
/// `D3DFVF_SPECULAR`: specular color (4 bytes; unused in slice 1).
pub const D3DFVF_SPECULAR: u32 = 0x0080;
/// `D3DFVF_TEX1`: first texture-coordinate set (2 floats; unused).
pub const D3DFVF_TEX1: u32 = 0x0100;
/// `D3DFVF_TEX8`: last texture-coordinate set (the `TEX1..TEX8` bit mask).
pub const D3DFVF_TEX8: u32 = 0x8000;

/// `D3DPT_POINTLIST`.
pub const D3DPT_POINTLIST: u32 = 1;
/// `D3DPT_LINELIST`.
pub const D3DPT_LINELIST: u32 = 2;
/// `D3DPT_LINESTRIP`.
pub const D3DPT_LINESTRIP: u32 = 3;
/// `D3DPT_TRIANGLELIST`.
pub const D3DPT_TRIANGLELIST: u32 = 4;
/// `D3DPT_TRIANGLESTRIP`.
pub const D3DPT_TRIANGLESTRIP: u32 = 5;
/// `D3DPT_TRIANGLEFAN`.
pub const D3DPT_TRIANGLEFAN: u32 = 6;

/// `D3DTS_VIEW`.
pub const D3DTS_VIEW: u32 = 2;
/// `D3DTS_PROJECTION`.
pub const D3DTS_PROJECTION: u32 = 3;
/// `D3DTS_WORLD` (world matrix index 0).
pub const D3DTS_WORLD: u32 = 256;
/// `D3DTS_TEXTURE0` — the first texture-space transform matrix.
pub const D3DTS_TEXTURE0: u32 = 16;
/// `D3DTS_TEXTURE7` — the last texture-space transform matrix.
pub const D3DTS_TEXTURE7: u32 = 23;

// ── Texture-stage constants (d3d9types.h values) ────────────────────────

/// `D3DTSS_COLOROP` (stage state type).
pub const D3DTSS_COLOROP: u32 = 1;
/// `D3DTSS_COLORARG1`.
pub const D3DTSS_COLORARG1: u32 = 2;
/// `D3DTSS_COLORARG2`.
pub const D3DTSS_COLORARG2: u32 = 3;
/// `D3DTSS_ALPHAOP`.
pub const D3DTSS_ALPHAOP: u32 = 4;
/// `D3DTSS_ALPHAARG1`.
pub const D3DTSS_ALPHAARG1: u32 = 5;
/// `D3DTSS_ALPHAARG2`.
pub const D3DTSS_ALPHAARG2: u32 = 6;
/// `D3DTSS_TEXCOORDINDEX`.
pub const D3DTSS_TEXCOORDINDEX: u32 = 11;

/// `D3DSAMP_ADDRESSU`.
pub const D3DSAMP_ADDRESSU: u32 = 1;
/// `D3DSAMP_ADDRESSV`.
pub const D3DSAMP_ADDRESSV: u32 = 2;
/// `D3DSAMP_MAGFILTER`.
pub const D3DSAMP_MAGFILTER: u32 = 5;
/// `D3DSAMP_MINFILTER`.
pub const D3DSAMP_MINFILTER: u32 = 6;
/// `D3DSAMP_MIPFILTER` (stored; mip mapping is deferred).
pub const D3DSAMP_MIPFILTER: u32 = 7;

/// `D3DTOP_DISABLE`.
pub const D3DTOP_DISABLE: u32 = 1;
/// `D3DTOP_SELECTARG1`.
pub const D3DTOP_SELECTARG1: u32 = 2;
/// `D3DTOP_SELECTARG2`.
pub const D3DTOP_SELECTARG2: u32 = 3;
/// `D3DTOP_MODULATE`.
pub const D3DTOP_MODULATE: u32 = 4;

/// `D3DTA_DIFFUSE`.
pub const D3DTA_DIFFUSE: u32 = 0;
/// `D3DTA_CURRENT` (stage 0 = the diffuse color).
pub const D3DTA_CURRENT: u32 = 1;
/// `D3DTA_TEXTURE`.
pub const D3DTA_TEXTURE: u32 = 2;

/// `D3DTADDRESS_WRAP`.
pub const D3DTADDRESS_WRAP: u32 = 1;
/// `D3DTADDRESS_CLAMP`.
pub const D3DTADDRESS_CLAMP: u32 = 3;

/// `D3DTEXF_POINT` (nearest).
pub const D3DTEXF_POINT: u32 = 1;
/// `D3DTEXF_LINEAR` (bilinear).
pub const D3DTEXF_LINEAR: u32 = 2;

// ── Blend + depth render-state constants (d3d9types.h values) ──────────

/// `D3DRS_ZENABLE`.
pub const D3DRS_ZENABLE: u32 = 7;
/// `D3DRS_ZWRITEENABLE`.
pub const D3DRS_ZWRITEENABLE: u32 = 14;
/// `D3DRS_SRCBLEND`.
pub const D3DRS_SRCBLEND: u32 = 19;
/// `D3DRS_DESTBLEND`.
pub const D3DRS_DESTBLEND: u32 = 20;
/// `D3DRS_ZFUNC`.
pub const D3DRS_ZFUNC: u32 = 23;
/// `D3DRS_ALPHABLENDENABLE`.
pub const D3DRS_ALPHABLENDENABLE: u32 = 27;
/// `D3DRS_BLENDOP`.
pub const D3DRS_BLENDOP: u32 = 171;

// ── L3 fragment-stage render-state constants (d3d9types.h values) ───────

/// `D3DRS_ALPHATESTENABLE` (the alpha-test gate).
pub const D3DRS_ALPHATESTENABLE: u32 = 15;
/// `D3DRS_ALPHAREF` (the alpha-test reference, a `u8`).
pub const D3DRS_ALPHAREF: u32 = 24;
/// `D3DRS_ALPHAFUNC` (the alpha-test compare function, `D3DCMP_*`).
pub const D3DRS_ALPHAFUNC: u32 = 25;
/// `D3DRS_FOGENABLE`.
pub const D3DRS_FOGENABLE: u32 = 28;
/// `D3DRS_FOGCOLOR` (a `D3DCOLOR`).
pub const D3DRS_FOGCOLOR: u32 = 34;
/// `D3DRS_FOGTABLEMODE` (pixel fog, a `D3DFOGMODE`).
pub const D3DRS_FOGTABLEMODE: u32 = 35;
/// `D3DRS_FOGSTART` (linear-fog depth start, a float).
pub const D3DRS_FOGSTART: u32 = 36;
/// `D3DRS_FOGEND` (linear-fog depth end, a float).
pub const D3DRS_FOGEND: u32 = 37;
/// `D3DRS_FOGDENSITY` (EXP/EXP2 fog density, a float).
pub const D3DRS_FOGDENSITY: u32 = 38;
/// `D3DRS_RANGEFOGENABLE` (a boolean; unmodeled — stored raw).
pub const D3DRS_RANGEFOGENABLE: u32 = 48;
/// `D3DRS_FOGVERTEXMODE` (vertex fog, a `D3DFOGMODE`).
pub const D3DRS_FOGVERTEXMODE: u32 = 50;
/// `D3DRS_SCISSORTESTENABLE` (the scissor-rect gate).
pub const D3DRS_SCISSORTESTENABLE: u32 = 174;

// ── L4 point/line render-state constants (d3d9types.h values) ──────────

/// `D3DRS_POINTSIZE` (a float, in device units; 1.0 = one pixel). Not a
/// modeled typed state — it rides the raw-value layer so `SetRenderState`
/// stores it and the point rasterizer reads the bits at draw time.
pub const D3DRS_POINTSIZE: u32 = 72;

/// `D3DFOG_NONE` — no fog.
pub const D3DFOG_NONE: u32 = 0;
/// `D3DFOG_EXP` — exponential fog.
pub const D3DFOG_EXP: u32 = 1;
/// `D3DFOG_EXP2` — exponential-squared fog.
pub const D3DFOG_EXP2: u32 = 2;
/// `D3DFOG_LINEAR` — linear fog (the demo path).
pub const D3DFOG_LINEAR: u32 = 3;

/// `D3DZB_FALSE` (0) — depth test+write disabled.
pub const D3DZB_FALSE: u32 = 0;
/// `D3DZB_TRUE` (1) — depth test+write enabled.
pub const D3DZB_TRUE: u32 = 1;
/// `D3DZB_USEW` (2) — depth write only, test disabled.
pub const D3DZB_USEW: u32 = 2;

/// `D3DCMP_NEVER`.
pub const D3DCMP_NEVER: u32 = 1;
/// `D3DCMP_LESS`.
pub const D3DCMP_LESS: u32 = 2;
/// `D3DCMP_EQUAL`.
pub const D3DCMP_EQUAL: u32 = 3;
/// `D3DCMP_LESSEQUAL`.
pub const D3DCMP_LESSEQUAL: u32 = 4;
/// `D3DCMP_GREATER`.
pub const D3DCMP_GREATER: u32 = 5;
/// `D3DCMP_NOTEQUAL`.
pub const D3DCMP_NOTEQUAL: u32 = 6;
/// `D3DCMP_GREATEREQUAL`.
pub const D3DCMP_GREATEREQUAL: u32 = 7;
/// `D3DCMP_ALWAYS`.
pub const D3DCMP_ALWAYS: u32 = 8;

/// `D3DBLEND_ZERO`.
pub const D3DBLEND_ZERO: u32 = 1;
/// `D3DBLEND_ONE`.
pub const D3DBLEND_ONE: u32 = 2;
/// `D3DBLEND_SRCCOLOR`.
pub const D3DBLEND_SRCCOLOR: u32 = 3;
/// `D3DBLEND_INVSRCCOLOR`.
pub const D3DBLEND_INVSRCCOLOR: u32 = 4;
/// `D3DBLEND_SRCALPHA`.
pub const D3DBLEND_SRCALPHA: u32 = 5;
/// `D3DBLEND_INVSRCALPHA`.
pub const D3DBLEND_INVSRCALPHA: u32 = 6;
/// `D3DBLEND_DESTALPHA`.
pub const D3DBLEND_DESTALPHA: u32 = 7;
/// `D3DBLEND_INVDESTALPHA`.
pub const D3DBLEND_INVDESTALPHA: u32 = 8;
/// `D3DBLEND_DESTCOLOR`.
pub const D3DBLEND_DESTCOLOR: u32 = 9;
/// `D3DBLEND_INVDESTCOLOR`.
pub const D3DBLEND_INVDESTCOLOR: u32 = 10;

/// `D3DBLENDOP_ADD`.
pub const D3DBLENDOP_ADD: u32 = 1;
/// `D3DBLENDOP_SUBTRACT`.
pub const D3DBLENDOP_SUBTRACT: u32 = 2;
/// `D3DBLENDOP_REVSUBTRACT`.
pub const D3DBLENDOP_REVSUBTRACT: u32 = 3;

/// Typed device render state (`D3DRS_*`), decoded at the `SetRenderState` /
/// `GetRenderState` register boundary and read by the per-draw fragment stage.
///
/// Unmodeled `D3DRS_*` states are carried verbatim by the handler's raw-value
/// layer (`D3D9State::d3d9_render_state_raw`), so `GetRenderState` round-trips
/// the last-set value for every state — modeled ones through these typed
/// fields, ignored ones through the raw map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderState {
    /// `D3DRS_ALPHABLENDENABLE`.
    pub alpha_blend_enable: bool,
    /// `D3DRS_ZWRITEENABLE`.
    pub z_write_enable: bool,
    /// `D3DRS_ZENABLE` (`D3DZB_*`).
    pub z_enable: D3dZBufferType,
    /// `D3DRS_ZFUNC` (`D3DCMP_*`).
    pub z_func: D3dCmpFunc,
    /// `D3DRS_SRCBLEND` (`D3DBLEND_*`).
    pub src_blend: D3dBlend,
    /// `D3DRS_DESTBLEND` (`D3DBLEND_*`).
    pub dest_blend: D3dBlend,
    /// `D3DRS_BLENDOP` (`D3DBLENDOP_*`).
    pub blend_op: D3dBlendOp,
    // ── L3 fragment stages (fog / alpha test / scissor) ────────────────
    /// `D3DRS_FOGENABLE` — gates the fog blend in the fragment stage.
    pub fog_enable: bool,
    /// `D3DRS_FOGCOLOR` (a `D3DCOLOR`; the fragment stage masks to 0RGB).
    pub fog_color: u32,
    /// `D3DRS_FOGSTART` (linear-fog depth start).
    pub fog_start: f32,
    /// `D3DRS_FOGEND` (linear-fog depth end).
    pub fog_end: f32,
    /// `D3DRS_FOGDENSITY` (EXP/EXP2 density).
    pub fog_density: f32,
    /// `D3DRS_FOGTABLEMODE` (`D3DFOGMODE_*`; non-NONE = pixel fog, which
    /// takes precedence over vertex fog when both are set).
    pub fog_table_mode: u32,
    /// `D3DRS_FOGVERTEXMODE` (`D3DFOGMODE_*`; used when table mode is NONE).
    pub fog_vertex_mode: u32,
    /// `D3DRS_ALPHATESTENABLE` — gates the alpha test in the fragment stage.
    pub alpha_test_enable: bool,
    /// `D3DRS_ALPHAFUNC` (`D3DCMP_*`).
    pub alpha_func: D3dCmpFunc,
    /// `D3DRS_ALPHAREF` (the alpha-test reference).
    pub alpha_ref: u8,
    /// `D3DRS_SCISSORTESTENABLE` — gates the scissor clip in the fragment
    /// stage.
    pub scissor_test_enable: bool,
}

impl RenderState {
    /// The typed value of a modeled `D3DRS_*` state, or `None` for unmodeled
    /// states (those read from the raw-value layer).
    ///
    /// `GetRenderState` calls this after the handler's validation pass, so the
    /// typed enums hold only legal values.
    #[must_use]
    pub fn value_of(&self, state_id: u32) -> Option<u32> {
        match state_id {
            D3DRS_ALPHABLENDENABLE => Some(u32::from(self.alpha_blend_enable)),
            D3DRS_ZWRITEENABLE => Some(u32::from(self.z_write_enable)),
            D3DRS_ZENABLE => Some(self.z_enable.as_u32()),
            D3DRS_ZFUNC => Some(self.z_func.as_u32()),
            D3DRS_SRCBLEND => Some(self.src_blend.as_u32()),
            D3DRS_DESTBLEND => Some(self.dest_blend.as_u32()),
            D3DRS_BLENDOP => Some(self.blend_op.as_u32()),
            D3DRS_FOGENABLE => Some(u32::from(self.fog_enable)),
            D3DRS_FOGCOLOR => Some(self.fog_color),
            D3DRS_FOGSTART => Some(self.fog_start.to_bits()),
            D3DRS_FOGEND => Some(self.fog_end.to_bits()),
            D3DRS_FOGDENSITY => Some(self.fog_density.to_bits()),
            D3DRS_FOGTABLEMODE => Some(self.fog_table_mode),
            D3DRS_FOGVERTEXMODE => Some(self.fog_vertex_mode),
            D3DRS_ALPHATESTENABLE => Some(u32::from(self.alpha_test_enable)),
            D3DRS_ALPHAFUNC => Some(self.alpha_func.as_u32()),
            D3DRS_ALPHAREF => Some(u32::from(self.alpha_ref)),
            D3DRS_SCISSORTESTENABLE => Some(u32::from(self.scissor_test_enable)),
            _ => None,
        }
    }
}

impl Default for RenderState {
    /// D3D9 fixed-function defaults (the old per-state `render_state`
    /// fallback values).
    fn default() -> Self {
        Self {
            alpha_blend_enable: false,
            z_write_enable: true,
            z_enable: D3dZBufferType::False,
            z_func: D3dCmpFunc::LessEqual,
            src_blend: D3dBlend::One,
            dest_blend: D3dBlend::Zero,
            blend_op: D3dBlendOp::Add,
            fog_enable: false,
            fog_color: 0,
            fog_start: 0.0,
            fog_end: 1.0,
            fog_density: 1.0,
            fog_table_mode: D3DFOG_NONE,
            fog_vertex_mode: D3DFOG_NONE,
            alpha_test_enable: false,
            alpha_func: D3dCmpFunc::Always,
            alpha_ref: 0,
            scissor_test_enable: false,
        }
    }
}

/// The D3D9 render-state validation matrix: every modeled `D3DRS_*` state's
/// legal value range.
///
/// `SetRenderState` rejects out-of-range values with `D3DERR_INVALIDCALL` —
/// the honest response, never a silent accept. The blend/compare/op ranges
/// are the *implemented* subset (a factor the software pipeline would render
/// wrong is rejected rather than silently mis-rendered); boolean states take
/// only TRUE/FALSE; unmodeled states carry no validation (their raw values
/// round-trip untouched).
#[must_use]
pub fn render_state_value_valid(state_id: u32, value: u32) -> bool {
    match state_id {
        // Boolean states: only FALSE (0) / TRUE (1).
        D3DRS_ALPHABLENDENABLE
        | D3DRS_ZWRITEENABLE
        | D3DRS_FOGENABLE
        | D3DRS_RANGEFOGENABLE
        | D3DRS_ALPHATESTENABLE
        | D3DRS_SCISSORTESTENABLE => value <= 1,
        // D3DZB_* depth modes.
        D3DRS_ZENABLE => value <= D3DZB_USEW,
        // D3DCMP_* compare functions (NEVER..ALWAYS).
        D3DRS_ZFUNC | D3DRS_ALPHAFUNC => (D3DCMP_NEVER..=D3DCMP_ALWAYS).contains(&value),
        // The implemented D3DBLEND_* factors (ZERO..INVDESTCOLOR).
        D3DRS_SRCBLEND | D3DRS_DESTBLEND => {
            (D3DBLEND_ZERO..=D3DBLEND_INVDESTCOLOR).contains(&value)
        }
        // The implemented D3DBLENDOP_* ops (ADD..REVSUBTRACT).
        D3DRS_BLENDOP => (D3DBLENDOP_ADD..=D3DBLENDOP_REVSUBTRACT).contains(&value),
        // D3DFOGMODE_* (NONE..LINEAR).
        D3DRS_FOGTABLEMODE | D3DRS_FOGVERTEXMODE => value <= D3DFOG_LINEAR,
        // D3DRS_ALPHAREF is a `u8`.
        D3DRS_ALPHAREF => value <= 255,
        // Unmodeled states are stored raw without validation.
        _ => true,
    }
}
/// One texture stage's fixed-function state (stage index is the array slot).
///
/// Merges the two register namespaces: `SetTextureStageState` (`D3DTSS_*`,
/// color-blend state) and `SetSamplerState` (`D3DSAMP_*`, sampling state).
/// Their constants collide numerically (`D3DTSS_COLOROP == D3DSAMP_ADDRESSU
/// == 1`, …), so the per-call decode routes each to its own field. The legacy
/// d3d8 TSS address/filter aliases are gone in D3D9 — address modes and
/// filters live only in the sampler namespace here. Unmodeled slots are
/// preserved verbatim so the `Get*` round-trips still see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureStageState {
    /// `D3DTSS_COLOROP` (default `D3DTOP_MODULATE`).
    pub color_op: u32,
    /// `D3DTSS_COLORARG1` (default `D3DTA_TEXTURE`).
    pub color_arg1: u32,
    /// `D3DTSS_COLORARG2` (default `D3DTA_DIFFUSE`).
    pub color_arg2: u32,
    /// `D3DTSS_ALPHAOP` (default `D3DTOP_MODULATE`).
    pub alpha_op: u32,
    /// `D3DTSS_ALPHAARG1` (default `D3DTA_TEXTURE`).
    pub alpha_arg1: u32,
    /// `D3DTSS_ALPHAARG2` (default `D3DTA_DIFFUSE`).
    pub alpha_arg2: u32,
    /// `D3DTSS_TEXCOORDINDEX`.
    pub tex_coord_index: u32,
    /// `D3DSAMP_ADDRESSU` (default `D3DTADDRESS_WRAP`).
    pub address_u: u32,
    /// `D3DSAMP_ADDRESSV` (default `D3DTADDRESS_WRAP`).
    pub address_v: u32,
    /// `D3DSAMP_MAGFILTER` (default `D3DTEXF_POINT`).
    pub mag_filter: u32,
    /// `D3DSAMP_MINFILTER` (default `D3DTEXF_POINT`).
    pub min_filter: u32,
    /// `D3DSAMP_MIPFILTER` (default `D3DTEXF_POINT`).
    pub mip_filter: u32,
    /// Unmodeled `D3DTSS_*` slots `(slot, value)`, preserved for `Get*`.
    pub other_tss: Vec<(u32, u32)>,
    /// Unmodeled `D3DSAMP_*` slots `(slot, value)`, preserved for `Get*`.
    pub other_sampler: Vec<(u32, u32)>,
}

impl Default for TextureStageState {
    /// D3D9 stage-0 fixed-function defaults (the old `stage_state` fallback
    /// values).
    fn default() -> Self {
        Self {
            color_op: D3DTOP_MODULATE,
            color_arg1: D3DTA_TEXTURE,
            color_arg2: D3DTA_DIFFUSE,
            alpha_op: D3DTOP_MODULATE,
            alpha_arg1: D3DTA_TEXTURE,
            alpha_arg2: D3DTA_DIFFUSE,
            tex_coord_index: 0,
            address_u: D3DTADDRESS_WRAP,
            address_v: D3DTADDRESS_WRAP,
            mag_filter: D3DTEXF_POINT,
            min_filter: D3DTEXF_POINT,
            mip_filter: D3DTEXF_POINT,
            other_tss: Vec::new(),
            other_sampler: Vec::new(),
        }
    }
}
/// Fill one triangle into `backbuffer` (0RGB, top-down, `width` × `height`)
/// with barycentric solid-fill, accumulating the covered region into `dirty`.
///
/// When `tex` is present and its `color_op` is not `D3DTOP_DISABLE`, each
/// fragment is textured: the uv is perspective-correct interpolated (the
/// per-vertex clip w divides the interpolant — the projective-correct value
/// that reproduces world-space linear uv), sampled with the stage's address
/// modes/filter, and combined with the Gouraud diffuse via the color op; the
/// alpha op (default MODULATE) yields the fragment alpha.
///
/// When `ps` is present (a pixel shader is bound), the fragment stage runs the
/// PS 2.0 interpreter instead of the FFP color/alpha ops: `oC0` feeds the
/// blend stage exactly where the FFP result would. `texkill` discards the
/// fragment (no color write, no depth write). Blend + depth apply
/// identically after the shader output is resolved.
///
/// `frag` carries the blend + depth configuration: the depth is
/// perspective-correct interpolated and tested/written per
/// `D3DRS_ZFUNC`/`ZWRITEENABLE` before the color write; then, when
/// `D3DRS_ALPHABLENDENABLE` is set, the fragment color blends over the
/// existing backbuffer pixel with the `D3DBLEND_*` factors.
/// The backbuffer stays 0RGB — alpha feeds the blend only.
///
/// The L3 fragment stages run in D3D9's order: the scissor rect
/// (`D3DRS_SCISSORTESTENABLE` + `SetScissorRect`) clips the pixel first, the
/// alpha test (`D3DRS_ALPHATESTENABLE`/`ALPHAFUNC`/`ALPHAREF`) discards
/// failing fragments after the color ops, and the fog blend
/// (`D3DRS_FOGENABLE` + the fog color/start/end/density, vertex fog from the
/// L1 stage's screen-space z or pixel fog from the interpolated z) tints the
/// color before the write.
///
/// Degenerate (zero-area) triangles and triangles fully off-screen are
/// dropped. The dirty region is the triangle's clipped bounding box —
/// conservative but always a superset of the written pixels.
///
/// `#[expect(casts)]`: pixel coordinates are `i32`/`u32` but the edge
/// functions run in float — `std` has no lossless int↔float `From`, and the
/// bounds are clamped to the backbuffer before any narrowing.
// Wide signature: a triangle draw carries 3 vertices + pipeline stages + target.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_triangle(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    a: ScreenVertex,
    b: ScreenVertex,
    c: ScreenVertex,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    // Signed area; zero = degenerate (collinear or zero-size triangle).
    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if area == 0.0 {
        return;
    }
    // Normalize the winding so the interior is on the left of every directed
    // edge (`e >= 0` for all three), then apply the top-left boundary rule.
    let (a, b, c) = if area < 0.0 { (a, c, b) } else { (a, b, c) };

    // L4 mip LOD: the screen-space uv gradient of the (winding-normalized)
    // triangle, measured in level-0 texels per pixel. Affine — exact for the
    // w≈1 orthographic draws the FFP path produces; a documented approximation
    // for perspective triangles (the true gradient varies per-pixel there).
    // `area_abs` is the barycentric denominator of the normalized triangle.
    let lod = if let Some(stage) = tex {
        let area_abs = area.abs();
        if area_abs > 1.0e-9 {
            let (aw, bw, cw) = (a, b, c);
            let du_dx =
                (aw.u * (cw.y - bw.y) + bw.u * (aw.y - cw.y) + cw.u * (bw.y - aw.y)) / area_abs;
            let du_dy =
                (aw.u * (bw.x - cw.x) + bw.u * (cw.x - aw.x) + cw.u * (aw.x - bw.x)) / area_abs;
            let dv_dx =
                (aw.v * (cw.y - bw.y) + bw.v * (aw.y - cw.y) + cw.v * (bw.y - aw.y)) / area_abs;
            let dv_dy =
                (aw.v * (bw.x - cw.x) + bw.v * (cw.x - aw.x) + cw.v * (aw.x - bw.x)) / area_abs;
            let w0 = stage.width as f32;
            let h0 = stage.height as f32;
            let footprint = (du_dx.abs() * w0)
                .max(du_dy.abs() * w0)
                .max(dv_dx.abs() * h0)
                .max(dv_dy.abs() * h0);
            footprint.max(1.0e-6).log2()
        } else {
            0.0
        }
    } else {
        0.0
    };

    let min_x = a.x.min(b.x).min(c.x).floor() as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil() as i32;
    let min_y = a.y.min(b.y).min(c.y).floor() as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil() as i32;
    let w_i = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = min_x.max(0);
    let y0 = min_y.max(0);
    let x1 = max_x.min(w_i);
    let y1 = max_y.min(h_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // D3DZB_USEW (2): write-only depth — test disabled, writes enabled.
    let depth_testing = frag.z_enable != 0 && frag.z_enable != D3DZB_USEW;
    let depth_writing = frag.z_enable != 0 && frag.z_write != 0;
    let blending = frag.alpha_blend != 0;
    let width_us = usize::try_from(width).unwrap_or(0);
    // P5b NEON fast paths (byte-identical; the D3D9_RESTING_FRAME_HASH gate
    // proves gui_d3d9's flat/blended quads hit them without changing pixels):
    // - `fast_blend`: SRCALPHA/INVSRCALPHA/ADD → `blend_0rgb_4x` over runs of
    //   4 contiguous accepted pixels.
    // - `flat_fill`: no blend, no texture, no PS, and all three vertex colors
    //   equal → the Gouraud result is provably the vertex color (weight sum
    //   error ≪ 0.5), so `fill_0rgb_4x` writes the constant color.
    let fast_blend = blending
        && frag.src_blend == D3DBLEND_SRCALPHA
        && frag.dest_blend == D3DBLEND_INVSRCALPHA
        && frag.blend_op == D3DBLENDOP_ADD;
    // L3: the fragment stages gate `flat_fill` deliberately — fog blends a
    // per-pixel color (the constant vertex color would be wrong), and the
    // alpha test / scissor reject pixels whose color this fast path bypasses
    // (the batch mechanics would still be correct, but the fast path's
    // semantics only stay obvious when every fragment pipeline stage is off).
    let flat_fill = !blending
        && frag.fog_enable == 0
        && frag.alpha_test == 0
        && frag.scissor_test == 0
        && tex.is_none()
        && ps.is_none()
        && a.color == b.color
        && b.color == c.color;
    let flat_color = a.color & 0x00FF_FFFF;
    let flat_alpha = u8::try_from((a.color >> 24) & 0xFF).unwrap_or(0);
    // L3 vertex fog: the factor is computed per-vertex from the L1 stage's
    // screen-space z and interpolated across the triangle (Gouraud). Pixel
    // fog (`D3DRS_FOGTABLEMODE` non-NONE) takes precedence and computes the
    // factor from the interpolated z per-pixel instead.
    let vertex_fog = frag.fog_enable != 0
        && frag.fog_table_mode == D3DFOG_NONE
        && frag.fog_vertex_mode != D3DFOG_NONE;
    let (fog_fa, fog_fb, fog_fc) = if vertex_fog {
        (
            fog_factor(frag, frag.fog_vertex_mode, a.z),
            fog_factor(frag, frag.fog_vertex_mode, b.z),
            fog_factor(frag, frag.fog_vertex_mode, c.z),
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    for py in y0..y1 {
        // P5b pending batch: up to 4 contiguous accepted pixels (index,
        // fragment color, alpha) flushed by the NEON kernel or the scalar
        // fallback. Reset per row — a batch never crosses rows.
        let mut batch_idx = [0usize; 4];
        let mut batch_rgb = [0u32; 4];
        let mut batch_alpha = [0u8; 4];
        let mut batch_n = 0usize;
        for px in x0..x1 {
            // L3 scissor test: the D3D9 rasterization clip, applied before any
            // fragment processing (including the depth test).
            if frag.scissor_test != 0
                && let Some(rect) = frag.scissor
                && (px < rect.left || px >= rect.right || py < rect.top || py >= rect.bottom)
            {
                continue;
            }
            let cx = px as f32 + 0.5;
            let cy = py as f32 + 0.5;
            let e_ab = (b.x - a.x) * (cy - a.y) - (b.y - a.y) * (cx - a.x);
            let e_bc = (c.x - b.x) * (cy - b.y) - (c.y - b.y) * (cx - b.x);
            let e_ca = (a.x - c.x) * (cy - c.y) - (a.y - c.y) * (cx - c.x);
            let inside = edge_inside(e_ab, b.x - a.x, b.y - a.y)
                && edge_inside(e_bc, c.x - b.x, c.y - b.y)
                && edge_inside(e_ca, a.x - c.x, a.y - c.y);
            if !inside {
                continue;
            }
            let sum = e_ab + e_bc + e_ca;
            if sum == 0.0 {
                continue;
            }
            let wa = e_bc / sum;
            let wb = e_ca / sum;
            let wc = e_ab / sum;
            let index = usize::try_from(py)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(px).unwrap_or(0));

            // Perspective-correct depth + attributes: interpolate
            // `attr/w` and `1/w` in screen space, then divide — the
            // projective-correct interpolant that reproduces the world-space
            // linear value (reduces to affine when w is constant, e.g.
            // XYZRHW w=1.0 or an orthographic projection). The transform
            // paths guarantee w > 0 (near-plane rejection); a direct
            // rasterize_triangle caller passing w <= 0 falls back to affine.
            let ia = if a.w > 0.0 { 1.0 / a.w } else { 1.0 };
            let ib = if b.w > 0.0 { 1.0 / b.w } else { 1.0 };
            let ic = if c.w > 0.0 { 1.0 / c.w } else { 1.0 };
            let iw = wa * ia + wb * ib + wc * ic;
            let perspective = |attr_a: f32, attr_b: f32, attr_c: f32| {
                if iw != 0.0 {
                    (wa * attr_a * ia + wb * attr_b * ib + wc * attr_c * ic) / iw
                } else {
                    wa * attr_a + wb * attr_b + wc * attr_c
                }
            };
            // Depth test + write (before the color write, per pixel).
            let z = perspective(a.z, b.z, c.z);
            if let Some(depth) = frag.depth.as_deref_mut() {
                let existing = depth.get(index).copied().unwrap_or(1.0);
                if depth_testing && !depth_test(z, existing, frag.z_func) {
                    continue; // discarded: no color write, no z write
                }
                if depth_writing && let Some(slot) = depth.get_mut(index) {
                    *slot = z;
                }
            }

            let (rgb, alpha) = if flat_fill {
                (flat_color, flat_alpha)
            } else {
                let gcolor = blend_colors(wa, a.color, wb, b.color, wc, c.color);
                let u = perspective(a.u, b.u, c.u);
                let v = perspective(a.v, b.v, c.v);
                match ps {
                    Some(program) => {
                        // Pixel shader path: v0 = interpolated diffuse, t0 = the
                        // affine-interpolated texture coordinate set 0. The
                        // shader's oC0 feeds the blend stage like the FFP result.
                        let input = PsFragmentInput {
                            v0: color_to_float4(gcolor),
                            v1: [0.0; 4],
                            t0: [u, v, 0.0, 1.0],
                        };
                        match run_pixel_shader(program, &input) {
                            Some(oc0) => (
                                pixel_shader_color_to_0rgb(oc0),
                                pixel_shader_alpha_to_u8(oc0),
                            ),
                            // texkill discarded the fragment: no color, no depth.
                            None => continue,
                        }
                    }
                    None => match tex {
                        Some(stage) if stage.color_op != D3DTOP_DISABLE => {
                            let texel = sample_texture_mip(stage, u, v, lod);
                            let arg1 = stage_arg(stage.color_arg1, texel, gcolor);
                            let arg2 = stage_arg(stage.color_arg2, texel, gcolor);
                            let rgb = eval_color_op(stage.color_op, arg1, arg2);
                            let alpha_arg1 = stage_arg(stage.alpha_arg1, texel, gcolor);
                            let alpha_arg2 = stage_arg(stage.alpha_arg2, texel, gcolor);
                            let alpha = eval_alpha_op(stage.alpha_op, alpha_arg1, alpha_arg2);
                            (rgb, alpha)
                        }
                        _ => (
                            gcolor & 0x00FF_FFFF,
                            u8::try_from((gcolor >> 24) & 0xFF).unwrap_or(0),
                        ),
                    },
                }
            };

            // ── L3 fragment stages: alpha test, then fog, before the write.
            // Alpha test: the fragment alpha vs D3DRS_ALPHAFUNC/ALPHAREF. A
            // failed test discards the fragment (no color write) — the `continue`
            // also breaks a pending NEON batch, so the fast paths stay correct.
            if frag.alpha_test != 0 && !alpha_test_pass(alpha, frag.alpha_func, frag.alpha_ref) {
                continue;
            }
            // Fog: blend the color toward D3DRS_FOGCOLOR by the factor —
            // pixel fog (FOGTABLEMODE) from the interpolated z, vertex fog
            // (FOGVERTEXMODE) from the interpolated per-vertex factors.
            let rgb = if frag.fog_enable != 0 {
                let factor = if frag.fog_table_mode != D3DFOG_NONE {
                    fog_factor(frag, frag.fog_table_mode, z)
                } else {
                    wa * fog_fa + wb * fog_fb + wc * fog_fc
                };
                fog_blend(rgb, frag.fog_color & 0x00FF_FFFF, factor)
            } else {
                rgb
            };

            if fast_blend || flat_fill {
                // Batch: the kernel needs 4 CONTIGUOUS accepted pixels, so a
                // rejected pixel (a `continue` above) or a row end breaks the
                // run — the next accepted pixel's non-contiguous index
                // flushes the pending batch via the scalar fallback.
                let prev = batch_idx.get(batch_n.wrapping_sub(1)).copied().unwrap_or(0);
                if batch_n == 4 || (batch_n > 0 && index != prev.saturating_add(1)) {
                    flush_pixel_batch(
                        backbuffer,
                        &batch_rgb,
                        batch_alpha,
                        &batch_idx,
                        batch_n,
                        fast_blend,
                        flat_fill,
                        flat_color,
                        blending,
                        frag,
                    );
                    batch_n = 0;
                }
                if let Some(slot) = batch_idx.get_mut(batch_n) {
                    *slot = index;
                }
                if let Some(slot) = batch_rgb.get_mut(batch_n) {
                    *slot = rgb;
                }
                if let Some(slot) = batch_alpha.get_mut(batch_n) {
                    *slot = alpha;
                }
                batch_n = batch_n.saturating_add(1);
            } else {
                let color = if blending {
                    let dst = backbuffer.get(index).copied().unwrap_or(0);
                    blend_fragment(dst, rgb, alpha, frag)
                } else {
                    rgb
                };
                if let Some(pixel) = backbuffer.get_mut(index) {
                    *pixel = color;
                }
            }
        }
        // Row end: flush the pending batch (partial run → scalar fallback).
        flush_pixel_batch(
            backbuffer,
            &batch_rgb,
            batch_alpha,
            &batch_idx,
            batch_n,
            fast_blend,
            flat_fill,
            flat_color,
            blending,
            frag,
        );
    }

    let rect = IRect {
        left: x0,
        top: y0,
        right: x1,
        bottom: y1,
    };
    // Conservative dirty region: the clipped bounding box, unioned into the
    // accumulated region (a full-dirty frame stays full).
    union_dirty_rect(dirty, rect);
}
/// Union one rect into the accumulated dirty region (None = full frame stays
/// full — the union of any rect with the full frame is the full frame).
fn union_dirty_rect(dirty: &mut Option<IRect>, rect: IRect) {
    *dirty = (*dirty).map(|prev| IRect {
        left: prev.left.min(rect.left),
        top: prev.top.min(rect.top),
        right: prev.right.max(rect.right),
        bottom: prev.bottom.max(rect.bottom),
    });
}
/// P5b: flush a pending batch of up to 4 accepted pixels.
///
/// Byte-identical to the scalar per-pixel write path: only when the batch
/// holds 4 CONTIGUOUS pixels and a NEON fast path applies (`fast_blend` =
/// SRCALPHA/INVSRCALPHA/ADD, or `flat_fill` = constant color, no blend) does
/// the kernel write all four at once; otherwise each pixel takes the scalar
/// path (`blend_fragment` when blending, plain write otherwise).
// Wide signature: a 4-pixel batch carries color, coverage, and blend state.
#[allow(clippy::too_many_arguments)]
fn flush_pixel_batch(
    backbuffer: &mut [u32],
    rgb: &[u32; 4],
    alpha: [u8; 4],
    indices: &[usize; 4],
    n: usize,
    fast_blend: bool,
    flat_fill: bool,
    flat_color: u32,
    blending: bool,
    frag: &FragmentState<'_>,
) {
    if n == 0 {
        return;
    }
    let base = indices.first().copied().unwrap_or(0);
    let contiguous = n == 4
        && indices.get(1).copied().unwrap_or(0) == base.saturating_add(1)
        && indices.get(2).copied().unwrap_or(0) == base.saturating_add(2)
        && indices.get(3).copied().unwrap_or(0) == base.saturating_add(3);
    if contiguous && fast_blend {
        // Gather the 4 destination pixels, blend with the NEON kernel,
        // scatter back. `blend_0rgb_4x` is byte-identical to
        // `blend_fragment` for these factors (verified by unit test + the
        // D3D9 resting-frame hash gate).
        let mut dst = [0u32; 4];
        for (i, slot) in dst.iter_mut().enumerate() {
            *slot = backbuffer.get(base.saturating_add(i)).copied().unwrap_or(0);
        }
        blend_0rgb_4x(&mut dst, rgb, &alpha);
        for (i, px) in dst.iter().enumerate() {
            if let Some(pixel) = backbuffer.get_mut(base.saturating_add(i)) {
                *pixel = *px;
            }
        }
        return;
    }
    if contiguous && flat_fill {
        if let Some(span) = backbuffer.get_mut(base..base.saturating_add(4)) {
            fill_0rgb_4x(span, flat_color);
        }
        return;
    }
    // Scalar fallback — identical to the pre-batch write path.
    for i in 0..n {
        let index = indices.get(i).copied().unwrap_or(0);
        let c = if blending {
            let dst = backbuffer.get(index).copied().unwrap_or(0);
            blend_fragment(
                dst,
                rgb.get(i).copied().unwrap_or(0),
                alpha.get(i).copied().unwrap_or(0),
                frag,
            )
        } else {
            rgb.get(i).copied().unwrap_or(0)
        };
        if let Some(pixel) = backbuffer.get_mut(index) {
            *pixel = c;
        }
    }
}
/// Rasterize one triangle of already-transformed [`GuestVertex`]s.
///
/// `pre_transformed` (`XYZRHW`) vertices bypass the matrix/viewport and use
/// their X/Y as screen pixels directly; their z is clamped to `[0, 1]` and
/// mapped through the viewport's `MinZ..MaxZ` (D3D9 applies the viewport
/// z-transform to pre-transformed vertices too). `XYZ` vertices transform to
/// clip space and are near-plane clipped (Sutherland–Hodgman against
/// `w > NEAR_CLIP_W`) — a triangle straddling the near plane keeps its visible
/// part instead of rejecting the whole triangle.
// Wide signature: a triangle draw carries 3 vertices + transform + pipeline stages.
#[allow(clippy::too_many_arguments)]
pub fn draw_triangle(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    v0: GuestVertex,
    v1: GuestVertex,
    v2: GuestVertex,
    pre_transformed: bool,
    matrix: &Mat4,
    vp: &Viewport,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    if pre_transformed {
        // XYZRHW: already screen-space — no near-plane clip (negative RHW is
        // undefined in D3D9). The attributes interpolate affinely, so the
        // perspective divisor is 1.0 and the z is the viewport-mapped RHW z.
        let to_screen = |v: GuestVertex| ScreenVertex {
            x: v.x,
            y: v.y,
            z: vp.min_z + v.z.clamp(0.0, 1.0) * (vp.max_z - vp.min_z),
            w: 1.0,
            color: v.color,
            u: v.u,
            v: v.v,
        };
        let (a, b, c) = (to_screen(v0), to_screen(v1), to_screen(v2));
        rasterize_triangle(backbuffer, width, height, a, b, c, tex, ps, frag, dirty);
        return;
    }
    let clip = |v: GuestVertex| ClipVertex {
        pos: transform_point([v.x, v.y, v.z, v.w], matrix),
        color: v.color,
        u: v.u,
        v: v.v,
    };
    let clipped = clip_polygon_near(&[clip(v0), clip(v1), clip(v2)]);
    // Fan the clipped polygon: a triangle yields 3 or 4 vertices.
    if let Some(first) = clipped.first() {
        for pair in clipped.get(1..).unwrap_or(&[]).windows(2) {
            let Some(a) = screen_from_clip(first, vp) else {
                continue;
            };
            let Some(b) = screen_from_clip(&pair[0], vp) else {
                continue;
            };
            let Some(c) = screen_from_clip(&pair[1], vp) else {
                continue;
            };
            rasterize_triangle(backbuffer, width, height, a, b, c, tex, ps, frag, dirty);
        }
    }
}
/// Rasterize one screen-space point (D3DPT_POINTLIST element).
///
/// The point is a square of `point_size` device units (1.0 = one pixel)
/// centered on the vertex, drawn with the half-open right/bottom edge rule
/// (the point analogue of the triangle top-left rule). Every fragment runs
/// the same pipeline as a triangle fragment (scissor, depth, texture/PS,
/// alpha test, fog, blend) through [`shade_fragment`]. Points carry no uv
/// gradient, so the mip LOD is 0 (level 0).
// Wide signature: a point draw carries the vertex + pipeline stages + target.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_point(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    v: ScreenVertex,
    point_size: f32,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    let half = point_size.max(1.0) * 0.5;
    let left = (v.x - half).floor() as i32;
    let top = (v.y - half).floor() as i32;
    let right = (v.x + half).ceil() as i32;
    let bottom = (v.y + half).ceil() as i32;
    let w_i = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = right.min(w_i);
    let y1 = bottom.min(h_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    union_dirty_rect(
        dirty,
        IRect {
            left: x0,
            top: y0,
            right: x1,
            bottom: y1,
        },
    );
    let width_us = usize::try_from(width).unwrap_or(0);
    for py in y0..y1 {
        for px in x0..x1 {
            let cx = px as f32 + 0.5;
            let cy = py as f32 + 0.5;
            // Half-open right/bottom: a size-1 point at a half-integer vertex
            // covers exactly the pixel containing the vertex.
            if cx < v.x - half || cx >= v.x + half || cy < v.y - half || cy >= v.y + half {
                continue;
            }
            let index = usize::try_from(py)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(px).unwrap_or(0));
            shade_fragment(
                backbuffer, index, px, py, v.color, v.u, v.v, v.z, 0.0, tex, ps, frag,
            );
        }
    }
}
/// Rasterize one screen-space line segment (a D3DPT_LINELIST/LINESTRIP
/// element) as a 1-pixel-wide line.
///
/// D3D9 has no line-width state (D3DRS_LINEWIDTH was dropped after D3D8), so
/// the line is always 1px: a pixel is covered when its center is within
/// 0.5px of the segment (the diamond-exit approximation). Attributes
/// interpolate along the segment with the same perspective-correct rule the
/// triangle path uses, and the mip LOD comes from the segment's uv gradient.
// Wide signature: a line draw carries 2 vertices + pipeline stages + target.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_line(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    a: ScreenVertex,
    b: ScreenVertex,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len2 = dx * dx + dy * dy;
    let min_x = a.x.min(b.x).floor() as i32;
    let max_x = a.x.max(b.x).ceil() as i32;
    let min_y = a.y.min(b.y).floor() as i32;
    let max_y = a.y.max(b.y).ceil() as i32;
    let w_i = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = min_x.max(0);
    let y0 = min_y.max(0);
    let x1 = max_x.min(w_i);
    let y1 = max_y.min(h_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    union_dirty_rect(
        dirty,
        IRect {
            left: x0,
            top: y0,
            right: x1,
            bottom: y1,
        },
    );
    // The mip LOD from the segment's uv gradient (texels per screen pixel
    // along the segment's dominant axis).
    let lod = match tex {
        Some(stage) if len2 > 0.0 => {
            let seg_len = len2.sqrt();
            let footprint = ((b.u - a.u).abs() * stage.width as f32)
                .max((b.v - a.v).abs() * stage.height as f32)
                / seg_len;
            footprint.max(1.0e-6).log2()
        }
        _ => 0.0,
    };
    // Perspective-correct line interpolation (the triangle-path rule).
    let ia = if a.w > 0.0 { 1.0 / a.w } else { 1.0 };
    let ib = if b.w > 0.0 { 1.0 / b.w } else { 1.0 };
    let width_us = usize::try_from(width).unwrap_or(0);
    for py in y0..y1 {
        for px in x0..x1 {
            let cx = px as f32 + 0.5;
            let cy = py as f32 + 0.5;
            // Nearest point on the segment; distance <= 0.5px covers the pixel.
            let t = if len2 > 0.0 {
                ((cx - a.x) * dx + (cy - a.y) * dy) / len2
            } else {
                0.0
            };
            let t = t.clamp(0.0, 1.0);
            let on_x = a.x + t * dx;
            let on_y = a.y + t * dy;
            let dist2 = (cx - on_x) * (cx - on_x) + (cy - on_y) * (cy - on_y);
            if dist2 > 0.25 {
                continue;
            }
            let iw = (1.0 - t) * ia + t * ib;
            let interp = |va: f32, vb: f32| {
                if iw != 0.0 {
                    ((1.0 - t) * va * ia + t * vb * ib) / iw
                } else {
                    (1.0 - t) * va + t * vb
                }
            };
            let index = usize::try_from(py)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(px).unwrap_or(0));
            shade_fragment(
                backbuffer,
                index,
                px,
                py,
                blend_colors(1.0 - t, a.color, t, b.color, 0.0, 0),
                interp(a.u, b.u),
                interp(a.v, b.v),
                interp(a.z, b.z),
                lod,
                tex,
                ps,
                frag,
            );
        }
    }
}
/// The shared point/line fragment pipeline: scissor → depth → color
/// (texture/PS/flat) → alpha test → fog → blend → write, in D3D9's order.
///
/// This is the per-pixel body the triangle path inlines (the triangle loop
/// keeps its batched NEON fast paths; points/lines are not hot enough for
/// them). `lod` selects the mip level for the texture sample.
// Wide signature: one fragment carries its attributes + pipeline stages.
#[allow(clippy::too_many_arguments)]
fn shade_fragment(
    backbuffer: &mut [u32],
    index: usize,
    px: i32,
    py: i32,
    gcolor: u32,
    u: f32,
    v: f32,
    z: f32,
    lod: f32,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
) {
    // L3 scissor test: the D3D9 rasterization clip, before the depth test.
    if frag.scissor_test != 0
        && let Some(rect) = frag.scissor
        && (px < rect.left || px >= rect.right || py < rect.top || py >= rect.bottom)
    {
        return;
    }
    let depth_testing = frag.z_enable != 0 && frag.z_enable != D3DZB_USEW;
    let depth_writing = frag.z_enable != 0 && frag.z_write != 0;
    if let Some(depth) = frag.depth.as_deref_mut() {
        let existing = depth.get(index).copied().unwrap_or(1.0);
        if depth_testing && !depth_test(z, existing, frag.z_func) {
            return; // discarded: no color write, no z write
        }
        if depth_writing && let Some(slot) = depth.get_mut(index) {
            *slot = z;
        }
    }
    let (rgb, alpha) = match ps {
        Some(program) => {
            // Pixel shader path (the triangle path's v0/t0 wiring).
            let input = PsFragmentInput {
                v0: color_to_float4(gcolor),
                v1: [0.0; 4],
                t0: [u, v, 0.0, 1.0],
            };
            match run_pixel_shader(program, &input) {
                Some(oc0) => (
                    pixel_shader_color_to_0rgb(oc0),
                    pixel_shader_alpha_to_u8(oc0),
                ),
                // texkill discarded the fragment: no color, no depth.
                None => return,
            }
        }
        None => match tex {
            Some(stage) if stage.color_op != D3DTOP_DISABLE => {
                let texel = sample_texture_mip(stage, u, v, lod);
                let arg1 = stage_arg(stage.color_arg1, texel, gcolor);
                let arg2 = stage_arg(stage.color_arg2, texel, gcolor);
                let rgb = eval_color_op(stage.color_op, arg1, arg2);
                let alpha_arg1 = stage_arg(stage.alpha_arg1, texel, gcolor);
                let alpha_arg2 = stage_arg(stage.alpha_arg2, texel, gcolor);
                (rgb, eval_alpha_op(stage.alpha_op, alpha_arg1, alpha_arg2))
            }
            _ => (
                gcolor & 0x00FF_FFFF,
                u8::try_from((gcolor >> 24) & 0xFF).unwrap_or(0),
            ),
        },
    };
    // L3 alpha test, then fog, before the write (the triangle-path order).
    if frag.alpha_test != 0 && !alpha_test_pass(alpha, frag.alpha_func, frag.alpha_ref) {
        return;
    }
    let rgb = if frag.fog_enable != 0 {
        let factor = if frag.fog_table_mode != D3DFOG_NONE {
            fog_factor(frag, frag.fog_table_mode, z)
        } else {
            fog_factor(frag, frag.fog_vertex_mode, z)
        };
        fog_blend(rgb, frag.fog_color & 0x00FF_FFFF, factor)
    } else {
        rgb
    };
    let color = if frag.alpha_blend != 0 {
        let dst = backbuffer.get(index).copied().unwrap_or(0);
        blend_fragment(dst, rgb, alpha, frag)
    } else {
        rgb
    };
    if let Some(pixel) = backbuffer.get_mut(index) {
        *pixel = color;
    }
}
