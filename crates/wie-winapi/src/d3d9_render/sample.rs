//! Texture-stage state, sampler address/filter resolution, and the
//! fixed-function color/alpha ops.

use super::{
    D3DCOLOR_RGB_MASK, D3DTA_CURRENT, D3DTA_DIFFUSE, D3DTA_TEXTURE, D3DTADDRESS_WRAP,
    D3DTEXF_LINEAR, D3DTOP_MODULATE, D3DTOP_SELECTARG2,
};

/// Maximum number of mip levels a stage carries. A 4096² texture chains to 13
/// levels, so 16 covers the whole range without the sampler sizing dynamically.
pub const MAX_MIP_LEVELS: usize = 16;

/// One mip level's host texels (`0xAARRGGBB`) plus its dimensions.
#[derive(Debug, Clone, Copy)]
pub struct MipLevelView<'a> {
    /// Level width in texels.
    pub width: u32,
    /// Level height in texels.
    pub height: u32,
    /// Level texels, row-major, top row first.
    pub pixels: &'a [u32],
}

/// The resolved mip chain a stage samples. Level 0 duplicates the stage's
/// full-res `pixels`/`width`/`height`; levels 1.. are the halved chain.
#[derive(Debug, Clone, Copy)]
pub struct MipChain<'a> {
    /// Live level count (1 = no mip chain — the pre-L4 slice).
    pub count: u32,
    /// Per-level views; slots `count..` are dead (always `None`).
    pub levels: [Option<MipLevelView<'a>>; MAX_MIP_LEVELS],
}

/// Resolved stage-0 texture state for one draw: the bound texture's texels
/// plus the sampler/color-op configuration that the fragment stage evaluates.
#[derive(Debug, Clone, Copy)]
pub struct TextureStage<'a> {
    /// Texels in `0xAARRGGBB` (D3DCOLOR) order, row-major, top row first.
    pub pixels: &'a [u32],
    /// Texture width in texels (level 0).
    pub width: u32,
    /// Texture height in texels (level 0).
    pub height: u32,
    /// U address mode (`D3DTADDRESS_WRAP` / `D3DTADDRESS_CLAMP`).
    pub addr_u: u32,
    /// V address mode.
    pub addr_v: u32,
    /// Magnification filter (`D3DTEXF_POINT` / `D3DTEXF_LINEAR`) — used when
    /// the texel footprint is ≤ 1 pixel (texels larger than pixels).
    pub mag_filter: u32,
    /// Minification filter — used when the footprint exceeds 1 pixel.
    pub min_filter: u32,
    /// Mip filter (`D3DTEXF_POINT` / `D3DTEXF_LINEAR`) — the level selection.
    pub mip_filter: u32,
    /// The mip chain (levels 0..`mips.count`; count 1 = single level).
    pub mips: MipChain<'a>,
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
///
/// The L3 fragment stages (fog, alpha test, scissor) ride here too: the
/// rasterizer applies them in D3D9's order — scissor clip first, then the
/// depth test, then the alpha test, then the fog blend, then the color blend.
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
    // ── L3 fragment stages (fog / alpha test / scissor) ────────────────
    /// `D3DRS_FOGENABLE` (bool as u32).
    pub fog_enable: u32,
    /// `D3DRS_FOGCOLOR` (D3DCOLOR; the fragment stage masks to 0RGB).
    pub fog_color: u32,
    /// `D3DRS_FOGSTART` — linear fog depth start.
    pub fog_start: f32,
    /// `D3DRS_FOGEND` — linear fog depth end.
    pub fog_end: f32,
    /// `D3DRS_FOGDENSITY` — EXP/EXP2 density.
    pub fog_density: f32,
    /// `D3DRS_FOGTABLEMODE` (`D3DFOGMODE_*`; non-NONE = pixel fog).
    pub fog_table_mode: u32,
    /// `D3DRS_FOGVERTEXMODE` (`D3DFOGMODE_*`; used when table mode is NONE).
    pub fog_vertex_mode: u32,
    /// `D3DRS_ALPHATESTENABLE` (bool as u32).
    pub alpha_test: u32,
    /// `D3DRS_ALPHAFUNC` (`D3DCMP_*`).
    pub alpha_func: u32,
    /// `D3DRS_ALPHAREF` (the alpha-test reference).
    pub alpha_ref: u8,
    /// `D3DRS_SCISSORTESTENABLE` (bool as u32).
    pub scissor_test: u32,
    /// The `SetScissorRect` rect in backbuffer coords (None = never set —
    /// the scissor test never clips).
    pub scissor: Option<crate::gdi32::IRect>,
}
/// Resolve a texel index from a raw (texel-space) index with the address mode.
///
/// `WRAP` mirrors the coordinate modulo the size; `CLAMP` (and any unknown
/// mode) saturates into `[0, size-1]`. `size` is at least 1.
#[must_use]
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
fn point_index(coord: f32, size: u32, address: u32) -> i32 {
    let size_f = size as f32;
    let raw = (coord * size_f.max(1.0)).floor() as i32;
    address_index(raw, size, address)
}
/// Fetch the texel at integer indices (addressed) from one mip level.
fn texel_at_view(stage: &TextureStage<'_>, view: &MipLevelView<'_>, x: i32, y: i32) -> u32 {
    let x = address_index(x, view.width, stage.addr_u);
    let y = address_index(y, view.height, stage.addr_v);
    let index = usize::try_from(y)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(view.width).unwrap_or(0))
        .saturating_add(usize::try_from(x).unwrap_or(0));
    view.pixels.get(index).copied().unwrap_or(0)
}
/// Sample one level's texel (point or bilinear) at a normalized `(u, v)`.
///
/// `#[expect(casts)]`: the bilinear weights are float math over `u8` channels
/// — `std` has no lossless float↔int `From`; results are clamped to 0..255.
#[must_use]
fn sample_level(
    stage: &TextureStage<'_>,
    view: &MipLevelView<'_>,
    u: f32,
    v: f32,
    linear: bool,
) -> u32 {
    if linear {
        let x = u * view.width as f32 - 0.5;
        let y = v * view.height as f32 - 0.5;
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let t00 = texel_at_view(stage, view, x0, y0);
        let t10 = texel_at_view(stage, view, x0 + 1, y0);
        let t01 = texel_at_view(stage, view, x0, y0 + 1);
        let t11 = texel_at_view(stage, view, x0 + 1, y0 + 1);
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
        let x = point_index(u, view.width, stage.addr_u);
        let y = point_index(v, view.height, stage.addr_v);
        texel_at_view(stage, view, x, y)
    }
}
/// Sample at the fragment's mip level: `level 0` of the chain.
///
/// The PS-interpreter path (`ps.rs`) calls this — its texld is fixed to the
/// full-res level; the FFP path uses [`sample_texture_mip`] with the LOD.
#[must_use]
pub(super) fn sample_texture(stage: &TextureStage<'_>, u: f32, v: f32) -> u32 {
    sample_texture_mip(stage, u, v, 0.0)
}
/// Sample `(u, v)` with the mip level selected by the fragment's LOD.
///
/// `lod` is `log2` of the level-0 texel footprint (0 = level 0 exactly).
/// `D3DTEXF_POINT` mip filtering rounds to the nearest level; `D3DTEXF_LINEAR`
/// blends the two bracketing levels (trilinear). The per-level filter is the
/// magnifier (`D3DSAMP_MAGFILTER`) when `lod <= 0` (texels bigger than
/// pixels) and the minifier (`D3DSAMP_MINFILTER`) otherwise.
#[must_use]
pub(super) fn sample_texture_mip(stage: &TextureStage<'_>, u: f32, v: f32, lod: f32) -> u32 {
    let count = stage.mips.count.max(1);
    let clamped = lod.clamp(0.0, (count - 1) as f32);
    if stage.mip_filter == D3DTEXF_LINEAR && count > 1 {
        let lower = clamped.floor() as u32;
        let upper = (lower + 1).min(count - 1);
        let frac = clamped - lower as f32;
        let a = sample_at_level(stage, lower, u, v, lod);
        let b = sample_at_level(stage, upper, u, v, lod);
        lerp_texel(a, b, frac)
    } else {
        let level = (clamped.round() as u32).min(count - 1);
        sample_at_level(stage, level, u, v, lod)
    }
}
/// Resolve one level's view and sample it with the filter for `lod`.
fn sample_at_level(stage: &TextureStage<'_>, level: u32, u: f32, v: f32, lod: f32) -> u32 {
    let fallback = MipLevelView {
        width: 1,
        height: 1,
        pixels: &[],
    };
    let view = if level == 0 {
        MipLevelView {
            width: stage.width,
            height: stage.height,
            pixels: stage.pixels,
        }
    } else {
        stage
            .mips
            .levels
            .get(level as usize)
            .and_then(|slot| *slot)
            .unwrap_or(fallback)
    };
    let linear = if lod <= 0.0 {
        stage.mag_filter
    } else {
        stage.min_filter
    } == D3DTEXF_LINEAR;
    sample_level(stage, &view, u, v, linear)
}
/// Per-channel lerp of two `0xAARRGGBB` texels (the trilinear blend).
#[must_use]
fn lerp_texel(a: u32, b: u32, frac: f32) -> u32 {
    let mix = |x: u8, y: u8| {
        let x = f32::from(x);
        let y = f32::from(y);
        (x + (y - x) * frac).round() as u8
    };
    let channel = |v: u32, shift: u32| u8::try_from((v >> shift) & 0xFF).unwrap_or(0);
    let r = mix(channel(a, 16), channel(b, 16));
    let g = mix(channel(a, 8), channel(b, 8));
    let bl = mix(channel(a, 0), channel(b, 0));
    let al = mix(channel(a, 24), channel(b, 24));
    (u32::from(al) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(bl)
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
        D3DTOP_SELECTARG2 => arg2 & D3DCOLOR_RGB_MASK,
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
        _ => arg1 & D3DCOLOR_RGB_MASK,
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
