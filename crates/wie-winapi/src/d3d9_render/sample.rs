//! Texture-stage state, sampler address/filter resolution, and the
//! fixed-function color/alpha ops.

use super::{
    D3DTA_CURRENT, D3DTA_DIFFUSE, D3DTA_TEXTURE, D3DTADDRESS_WRAP, D3DTEXF_LINEAR, D3DTOP_MODULATE,
    D3DTOP_SELECTARG2,
};

/// Resolved stage-0 texture state for one draw: the bound texture's texels
/// plus the sampler/color-op configuration that the fragment stage evaluates.
#[derive(Debug, Clone, Copy)]
pub struct TextureStage<'a> {
    /// Texels in `0xAARRGGBB` (D3DCOLOR) order, row-major, top row first.
    pub pixels: &'a [u32],
    /// Texture width in texels.
    pub width: u32,
    /// Texture height in texels.
    pub height: u32,
    /// U address mode (`D3DTADDRESS_WRAP` / `D3DTADDRESS_CLAMP`).
    pub addr_u: u32,
    /// V address mode.
    pub addr_v: u32,
    /// Sampling filter (`D3DTEXF_POINT` / `D3DTEXF_LINEAR`).
    pub mag_filter: u32,
    /// `D3DTSS_COLOROP` (`D3DTOP_MODULATE` / `SELECTARG1` / `SELECTARG2`).
    pub color_op: u32,
    /// `D3DTSS_COLORARG1` (`D3DTA_TEXTURE` / `D3DTA_DIFFUSE` / `D3DTA_CURRENT`).
    pub color_arg1: u32,
    /// `D3DTSS_COLORARG2`.
    pub color_arg2: u32,
    /// `D3DTSS_ALPHAOP` (default `D3DTOP_MODULATE`).
    pub alpha_op: u32,
    /// `D3DTSS_ALPHAARG1` (default `D3DTA_TEXTURE`).
    pub alpha_arg1: u32,
    /// `D3DTSS_ALPHAARG2` (default `D3DTA_DIFFUSE`).
    pub alpha_arg2: u32,
}
/// Resolved blend + depth state for one draw.
///
/// `depth` is the bound depth buffer (None = no depth surface). The backbuffer
/// stays 0RGB — the fragment alpha feeds the blend factors only.
#[derive(Debug)]
pub struct FragmentState<'a> {
    /// Bound depth buffer texels (None = no depth surface).
    pub depth: Option<&'a mut [f32]>,
    /// `D3DRS_ZENABLE` (0 = off, 1 = test+write, 2 = write only).
    pub z_enable: u32,
    /// `D3DRS_ZFUNC` (`D3DCMP_*`).
    pub z_func: u32,
    /// `D3DRS_ZWRITEENABLE` (bool as u32).
    pub z_write: u32,
    /// `D3DRS_ALPHABLENDENABLE` (bool as u32).
    pub alpha_blend: u32,
    /// `D3DRS_SRCBLEND` (`D3DBLEND_*`).
    pub src_blend: u32,
    /// `D3DRS_DESTBLEND`.
    pub dest_blend: u32,
    /// `D3DRS_BLENDOP` (`D3DBLENDOP_*`).
    pub blend_op: u32,
}
/// Resolve a texel index from a raw (texel-space) index with the address mode.
///
/// `WRAP` mirrors the coordinate modulo the size; `CLAMP` (and any unknown
/// mode) saturates into `[0, size-1]`. `size` is at least 1.
#[must_use]
#[expect(clippy::arithmetic_side_effects)] // mod/saturate on a bounded index
fn address_index(index: i32, size: u32, address: u32) -> i32 {
    let size_i = i32::try_from(size).unwrap_or(0).max(1);
    if address == D3DTADDRESS_WRAP {
        ((index % size_i) + size_i) % size_i
    } else {
        index.max(0).min(size_i - 1)
    }
}
/// Map a normalized texture coordinate to a texel index for point sampling.
///
/// `floor(u * size)` so the last texel (`u` just under 1.0) is reachable.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn point_index(coord: f32, size: u32, address: u32) -> i32 {
    let size_f = size as f32;
    let raw = (coord * size_f.max(1.0)).floor() as i32;
    address_index(raw, size, address)
}
/// Fetch the texel at integer indices (addressed).
fn texel_at(stage: &TextureStage<'_>, x: i32, y: i32) -> u32 {
    let x = address_index(x, stage.width, stage.addr_u);
    let y = address_index(y, stage.height, stage.addr_v);
    let index = usize::try_from(y)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(stage.width).unwrap_or(0))
        .saturating_add(usize::try_from(x).unwrap_or(0));
    stage.pixels.get(index).copied().unwrap_or(0)
}
/// Sample one texel (point or bilinear) at a normalized `(u, v)`.
///
/// `#[expect(casts)]`: the bilinear weights are float math over `u8` channels
/// — `std` has no lossless float↔int `From`; results are clamped to 0..255.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names
)]
pub(super) fn sample_texture(stage: &TextureStage<'_>, u: f32, v: f32) -> u32 {
    if stage.mag_filter == D3DTEXF_LINEAR {
        let x = u * stage.width as f32 - 0.5;
        let y = v * stage.height as f32 - 0.5;
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let t00 = texel_at(stage, x0, y0);
        let t10 = texel_at(stage, x0 + 1, y0);
        let t01 = texel_at(stage, x0, y0 + 1);
        let t11 = texel_at(stage, x0 + 1, y0 + 1);
        let lerp_channel = |a: u8, b: u8, f: f32| {
            let a = f32::from(a);
            let b = f32::from(b);
            (a + (b - a) * f).round() as u8
        };
        let top = [
            lerp_channel(
                u8::try_from((t00 >> 16) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 16) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t00 >> 8) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 8) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from(t00 & 0xFF).unwrap_or(0),
                u8::try_from(t10 & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t00 >> 24) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 24) & 0xFF).unwrap_or(0),
                fx,
            ),
        ];
        let bottom = [
            lerp_channel(
                u8::try_from((t01 >> 16) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 16) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t01 >> 8) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 8) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from(t01 & 0xFF).unwrap_or(0),
                u8::try_from(t11 & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t01 >> 24) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 24) & 0xFF).unwrap_or(0),
                fx,
            ),
        ];
        let mix = |a: u8, b: u8| {
            let a = f32::from(a);
            let b = f32::from(b);
            (a + (b - a) * fy).round() as u8
        };
        let r = mix(top[0], bottom[0]);
        let g = mix(top[1], bottom[1]);
        let b = mix(top[2], bottom[2]);
        let a = mix(top[3], bottom[3]);
        (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
    } else {
        let x = point_index(u, stage.width, stage.addr_u);
        let y = point_index(v, stage.height, stage.addr_v);
        texel_at(stage, x, y)
    }
}
/// Resolve a `D3DTA_*` color argument to an `0xAARRGGBB` value.
#[must_use]
pub(super) fn stage_arg(arg: u32, texel: u32, diffuse: u32) -> u32 {
    match arg {
        D3DTA_TEXTURE => texel,
        // Stage 0 has no previous-stage result, so CURRENT is the diffuse.
        D3DTA_DIFFUSE | D3DTA_CURRENT => diffuse,
        _ => 0,
    }
}
/// Evaluate a `D3DTSS_COLOROP` over two `0xAARRGGBB` args → 0RGB.
///
/// `MODULATE` multiplies the RGB channels (alpha byte dropped); unknown ops
/// fall back to `SELECTARG1` (documented — the common subset policy).
#[must_use]
pub(super) fn eval_color_op(op: u32, arg1: u32, arg2: u32) -> u32 {
    match op {
        D3DTOP_SELECTARG2 => arg2 & 0x00FF_FFFF,
        D3DTOP_MODULATE => {
            let r = (u32::from(u8::try_from((arg1 >> 16) & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from((arg2 >> 16) & 0xFF).unwrap_or(0)))
                >> 8;
            let g = (u32::from(u8::try_from((arg1 >> 8) & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from((arg2 >> 8) & 0xFF).unwrap_or(0)))
                >> 8;
            let b = (u32::from(u8::try_from(arg1 & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from(arg2 & 0xFF).unwrap_or(0)))
                >> 8;
            (r << 16) | (g << 8) | b
        }
        _ => arg1 & 0x00FF_FFFF,
    }
}
/// Evaluate a `D3DTSS_ALPHAOP` over two `0xAARRGGBB` args → the alpha byte.
///
/// `MODULATE` multiplies the alpha channels; unknown ops fall back to
/// `SELECTARG1` (documented — the common subset policy).
#[must_use]
pub(super) fn eval_alpha_op(op: u32, arg1: u32, arg2: u32) -> u8 {
    let a1 = u8::try_from((arg1 >> 24) & 0xFF).unwrap_or(0);
    let a2 = u8::try_from((arg2 >> 24) & 0xFF).unwrap_or(0);
    match op {
        D3DTOP_SELECTARG2 => a2,
        D3DTOP_MODULATE => {
            u8::try_from(u32::from(a1).saturating_mul(u32::from(a2)) >> 8).unwrap_or(0)
        }
        // SELECTARG1 and unknown ops both yield arg1.
        _ => a1,
    }
}
