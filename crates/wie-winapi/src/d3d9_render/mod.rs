//! P3: D3D9 software-render core — FVF layout parsing, vertex transform,
//! and solid-fill triangle rasterization.
//!
//! Slice 1 scope (roadmap B6 slice 1): `D3DFVF_XYZ` / `D3DFVF_XYZRHW`
//! positions, optional `D3DFVF_NORMAL` / `D3DFVF_DIFFUSE` / `D3DFVF_SPECULAR`,
//! flat-vertex diffuse colors, a fixed-function world × view × projection
//! transform, near-plane rejection, and a top-left-rule fill with barycentric
//! color interpolation. No textures, lighting, or z-buffer (draw order).
//! P5a adds the PS 2.0 interpreter in the fragment stage.
//!
//! Submodules: [`vertex`] (FVF layout + transform), [`sample`] (texture-stage
//! sampling + FFP color ops), [`blend`] (typed render-state enums, depth test,
//! alpha blend), [`ps`] (PS 2.0 interpreter).

mod blend;
mod ps;
mod sample;
mod vertex;

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

pub use self::blend::{D3dBlend, D3dBlendOp, D3dCmpFunc, D3dZBufferType, is_top_or_left_edge};
pub use self::ps::{
    PsFragmentInput, PsProgram, pixel_shader_alpha_to_u8, pixel_shader_color_to_0rgb,
    run_pixel_shader,
};
pub use self::sample::{FragmentState, TextureStage};
pub use self::vertex::{
    FvfLayout, GuestVertex, IDENTITY, Mat4, ScreenVertex, Viewport, clip_to_screen, mat4_mul,
    parse_fvf, parse_vertex, transform_point,
};

// Private cross-module helpers: rasterize_triangle (below) drives the
// fragment stage through these, so each lives in the submodule that owns it.
use self::blend::{blend_colors, blend_fragment, depth_test, edge_inside};
use self::ps::color_to_float4;
use self::sample::{eval_alpha_op, eval_color_op, sample_texture, stage_arg};

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
/// Unmodeled `D3DRS_*` states are dropped by the handler (they have no effect
/// on the software pipeline and their `GetRenderState` reads fall back to 0,
/// D3D9's default for unused states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        }
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
/// fragment is textured: the uv is affine-interpolated (perspective-correct
/// interpolation deferred — fine for screen-aligned quads), sampled with the
/// stage's address modes/filter, and combined with the Gouraud diffuse via the
/// color op; the alpha op (default MODULATE) yields the fragment alpha.
///
/// When `ps` is present (a pixel shader is bound), the fragment stage runs the
/// PS 2.0 interpreter instead of the FFP color/alpha ops: `oC0` feeds the
/// blend stage exactly where the FFP result would. `texkill` discards the
/// fragment (no color write, no depth write). Blend + depth from P4c apply
/// identically after the shader output is resolved.
///
/// `frag` carries the blend + depth configuration: the depth is affine-
/// interpolated and tested/written per `D3DRS_ZFUNC`/`ZWRITEENABLE` before the
/// color write; then, when `D3DRS_ALPHABLENDENABLE` is set, the fragment color
/// blends over the existing backbuffer pixel with the `D3DBLEND_*` factors.
/// The backbuffer stays 0RGB — alpha feeds the blend only.
///
/// Degenerate (zero-area) triangles and triangles fully off-screen are
/// dropped. The dirty region is the triangle's clipped bounding box —
/// conservative but always a superset of the written pixels.
///
/// `#[expect(casts)]`: pixel coordinates are `i32`/`u32` but the edge
/// functions run in float — `std` has no lossless int↔float `From`, and the
/// bounds are clamped to the backbuffer before any narrowing.
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
    let flat_fill =
        !blending && tex.is_none() && ps.is_none() && a.color == b.color && b.color == c.color;
    let flat_color = a.color & 0x00FF_FFFF;
    let flat_alpha = u8::try_from((a.color >> 24) & 0xFF).unwrap_or(0);
    for py in y0..y1 {
        // P5b pending batch: up to 4 contiguous accepted pixels (index,
        // fragment color, alpha) flushed by the NEON kernel or the scalar
        // fallback. Reset per row — a batch never crosses rows.
        let mut batch_idx = [0usize; 4];
        let mut batch_rgb = [0u32; 4];
        let mut batch_alpha = [0u8; 4];
        let mut batch_n = 0usize;
        for px in x0..x1 {
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

            // Depth test + write (before the color write, per pixel).
            let z = wa * a.z + wb * b.z + wc * c.z;
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
                let u = wa * a.u + wb * b.u + wc * c.u;
                let v = wa * a.v + wb * b.v + wc * c.v;
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
                            let texel = sample_texture(stage, u, v);
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
/// their X/Y as screen pixels directly. Any vertex at/behind the near plane
/// rejects the whole triangle (slice 1 limitation — no near-plane clipping).
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
    let to_screen = |v: GuestVertex| -> Option<ScreenVertex> {
        let (sx, sy) = if pre_transformed {
            (v.x, v.y)
        } else {
            let clip = transform_point([v.x, v.y, v.z, v.w], matrix);
            clip_to_screen(clip, vp)?
        };
        Some(ScreenVertex {
            x: sx,
            y: sy,
            z: v.z,
            color: v.color,
            u: v.u,
            v: v.v,
        })
    };
    let Some(a) = to_screen(v0) else { return };
    let Some(b) = to_screen(v1) else { return };
    let Some(c) = to_screen(v2) else { return };
    rasterize_triangle(backbuffer, width, height, a, b, c, tex, ps, frag, dirty);
}
