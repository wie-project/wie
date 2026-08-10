// ── Texture sampling ────────────────────────────────────────────────────
//
// The texel fetch + filter helpers used by the fragment stage and the
// rasterizers. Pure host-side: textures are stored top-down in
// `0xAARRGGBB`, and GL's bottom-up upload rows are flipped at upload time.

use super::*;

// ── Texture sampling ────────────────────────────────────────────────────

/// Resolve a texel coordinate measured from the bottom/left (GL `(0,0)` =
/// the first upload row) to a top-down row / column index, honoring the wrap.
#[must_use]
fn address_index(n: i32, size: u32, wrap: bool) -> i32 {
    let size_i = i32::try_from(size).unwrap_or(1).max(1);
    if wrap {
        ((n % size_i) + size_i) % size_i
    } else {
        n.clamp(0, size_i - 1)
    }
}

/// Fetch one texel (`0xAARRGGBB`) at bottom-left-origin integer coords.
#[must_use]
fn texel_at(tex: &TextureObject, x: i32, y: i32) -> u32 {
    let w = usize::try_from(tex.width).unwrap_or(0);
    let h_i = i32::try_from(tex.height).unwrap_or(0).max(1);
    let row = (h_i - 1 - y).max(0);
    let index = usize::try_from(row)
        .unwrap_or(0)
        .saturating_mul(w)
        .saturating_add(usize::try_from(x.max(0)).unwrap_or(0));
    tex.pixels.get(index).copied().unwrap_or(0)
}

/// Sample `(u, v)` with the `GL_NEAREST` filter.
#[must_use]
fn sample_nearest(tex: &TextureObject, u: f32, v: f32) -> u32 {
    let wrap_s = tex.wrap_s == GL_REPEAT;
    let wrap_t = tex.wrap_t == GL_REPEAT;
    let x = address_index((u * tex.width as f32).floor() as i32, tex.width, wrap_s);
    let y = address_index((v * tex.height as f32).floor() as i32, tex.height, wrap_t);
    texel_at(tex, x, y)
}

/// Sample `(u, v)` with bilinear interpolation (`GL_LINEAR`).
#[must_use]
fn sample_linear(tex: &TextureObject, u: f32, v: f32) -> u32 {
    let wrap_s = tex.wrap_s == GL_REPEAT;
    let wrap_t = tex.wrap_t == GL_REPEAT;
    let xf = u * tex.width as f32 - 0.5;
    let yf = v * tex.height as f32 - 0.5;
    let x0 = xf.floor() as i32;
    let y0 = yf.floor() as i32;
    let fx = xf - x0 as f32;
    let fy = yf - y0 as f32;
    let ix0 = address_index(x0, tex.width, wrap_s);
    let ix1 = address_index(x0 + 1, tex.width, wrap_s);
    let iy0 = address_index(y0, tex.height, wrap_t);
    let iy1 = address_index(y0 + 1, tex.height, wrap_t);
    let t00 = texel_at(tex, ix0, iy0);
    let t10 = texel_at(tex, ix1, iy0);
    let t01 = texel_at(tex, ix0, iy1);
    let t11 = texel_at(tex, ix1, iy1);
    let mix = |a: u8, b: u8, f: f32| {
        let a = f32::from(a);
        let b = f32::from(b);
        (a + (b - a) * f).round() as u8
    };
    let channel = |t: u32, shift: u32| u8::try_from((t >> shift) & 0xFF).unwrap_or(0);
    let top_r = mix(channel(t00, 16), channel(t10, 16), fx);
    let top_g = mix(channel(t00, 8), channel(t10, 8), fx);
    let top_b = mix(channel(t00, 0), channel(t10, 0), fx);
    let top_a = mix(channel(t00, 24), channel(t10, 24), fx);
    let bot_r = mix(channel(t01, 16), channel(t11, 16), fx);
    let bot_g = mix(channel(t01, 8), channel(t11, 8), fx);
    let bot_b = mix(channel(t01, 0), channel(t11, 0), fx);
    let bot_a = mix(channel(t01, 24), channel(t11, 24), fx);
    let r = mix(top_r, bot_r, fy);
    let g = mix(top_g, bot_g, fy);
    let b = mix(top_b, bot_b, fy);
    let a = mix(top_a, bot_a, fy);
    (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

/// Sample a texel as RGBA (0..1), `linear` selects the filter.
#[must_use]
pub(super) fn sample_texel(tex: &TextureObject, u: f32, v: f32, linear: bool) -> [f32; 4] {
    let t = if linear {
        sample_linear(tex, u, v)
    } else {
        sample_nearest(tex, u, v)
    };
    unpack_color(t)
}

/// The texel footprint (texels-per-pixel) of a triangle's uv field — selects
/// the magnification filter when ≤ 1 and the minification filter otherwise.
#[must_use]
pub(super) fn texel_footprint(
    t: &TextureObject,
    a: ScreenVertex,
    b: ScreenVertex,
    c: ScreenVertex,
    area: f32,
) -> f32 {
    let area_abs = area.abs();
    if area_abs <= 1.0e-6 {
        return 1.0;
    }
    let du_dx = (a.u * (c.y - b.y) + b.u * (a.y - c.y) + c.u * (b.y - a.y)) / area_abs;
    let dv_dy = (a.v * (b.x - c.x) + b.v * (c.x - a.x) + c.v * (a.x - b.x)) / area_abs;
    (du_dx.abs() * t.width as f32)
        .max(dv_dy.abs() * t.height as f32)
        .max(1.0)
}
