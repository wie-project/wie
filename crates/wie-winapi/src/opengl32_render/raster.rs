// ── Rasterizers (scanline fill, 1-px DDA lines, points) ─────────────────
//
// Each takes a [`GlScreenVertex`] (the post-viewport vertex carrying the
// interpolated color/uv/depth AND the shader varying slots) and drives the
// shared per-fragment stage in `fragment.rs` (depth test → shader or FFP
// color → blend → store). Varying interpolation is perspective-correct via
// the clip w, exactly like the color/uv path.

use super::*;
use crate::d3d9_render::ScreenVertex;
use fragment::{edge_inside, write_fragment};

/// Maximum floats per vertex carried for varying interpolation.
pub(crate) const MAX_VARYING_FLOATS: usize = super::glsl::MAX_VARYING_FLOATS;

/// A post-viewport vertex (screen position + depth + color/uv + varyings).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlScreenVertex {
    pub x: f32,
    pub y: f32,
    /// Post-viewport depth (0..1, near..far).
    pub z: f32,
    /// Clip-space w (perspective-correct attribute interpolation).
    pub w: f32,
    /// Interpolated color `0xAARRGGBB`.
    pub color: u32,
    /// Texture coordinate U.
    pub u: f32,
    /// Texture coordinate V.
    pub v: f32,
    /// Shader varying slots (all zeros when no shader is active).
    pub varyings: [f32; MAX_VARYING_FLOATS],
}

pub(super) fn rasterize_point(ctx: &mut GlCtx, s: GlScreenVertex) {
    let [r, g, b, a] = unpack_color(s.color);
    let linear = ctx
        .textures
        .iter()
        .find(|t| t.id == ctx.bound_texture && !t.pixels.is_empty())
        .is_some_and(|t| t.mag_filter == GL_LINEAR);
    write_fragment(
        ctx,
        s.x.floor() as i32,
        s.y.floor() as i32,
        s.z,
        r,
        g,
        b,
        a,
        s.u,
        s.v,
        linear,
        &s.varyings,
    );
}

/// 1-pixel DDA line with per-pixel interpolated attributes.
pub(super) fn rasterize_line(ctx: &mut GlCtx, a: GlScreenVertex, b: GlScreenVertex) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let span = dx.abs().max(dy.abs());
    if span < 0.5 {
        rasterize_point(ctx, a);
        return;
    }
    let steps = span.ceil() as i32;
    let [ar, ag, ab, aa] = unpack_color(a.color);
    let [br, bg, bb, ba] = unpack_color(b.color);
    let linear = ctx
        .textures
        .iter()
        .find(|t| t.id == ctx.bound_texture && !t.pixels.is_empty())
        .is_some_and(|t| t.mag_filter == GL_LINEAR);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let mut varyings = [0.0_f32; MAX_VARYING_FLOATS];
        for (slot, dst) in varyings.iter_mut().enumerate() {
            let av = a.varyings.get(slot).copied().unwrap_or(0.0);
            let bv = b.varyings.get(slot).copied().unwrap_or(0.0);
            *dst = av + (bv - av) * t;
        }
        write_fragment(
            ctx,
            (a.x + dx * t).floor() as i32,
            (a.y + dy * t).floor() as i32,
            a.z + (b.z - a.z) * t,
            ar + (br - ar) * t,
            ag + (bg - ag) * t,
            ab + (bb - ab) * t,
            aa + (ba - aa) * t,
            a.u + (b.u - a.u) * t,
            a.v + (b.v - a.v) * t,
            linear,
            &varyings,
        );
    }
}

/// Scanline triangle fill: edge functions with the top-left rule, barycentric
/// interpolation of color/uv/depth + varying slots (perspective-correct via
/// the clip w).
pub(super) fn rasterize_triangle(
    ctx: &mut GlCtx,
    a: GlScreenVertex,
    b: GlScreenVertex,
    c: GlScreenVertex,
) {
    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if area == 0.0 {
        return;
    }
    // Normalize the winding so the interior lies on the left of every edge.
    let (a, b, c) = if area < 0.0 { (a, c, b) } else { (a, b, c) };
    let min_x = a.x.min(b.x).min(c.x).floor() as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil() as i32;
    let min_y = a.y.min(b.y).min(c.y).floor() as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil() as i32;
    let w_i = i32::try_from(ctx.width).unwrap_or(0);
    let h_i = i32::try_from(ctx.height).unwrap_or(0);
    let x0 = min_x.max(0);
    let y0 = min_y.max(0);
    let x1 = max_x.min(w_i);
    let y1 = max_y.min(h_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let [ar, ag, ab, aa] = unpack_color(a.color);
    let [br, bg, bb, ba] = unpack_color(b.color);
    let [cr, cg, cb, ca] = unpack_color(c.color);
    // Mag vs min filter from the uv texel footprint (approximated per
    // triangle — exact for the w≈1 orthographic draws the fixed-function
    // path produces).
    let linear = if ctx.texture_2d {
        ctx.textures
            .iter()
            .find(|t| t.id == ctx.bound_texture && !t.pixels.is_empty())
            .is_some_and(|t| {
                let footprint = texel_footprint(t, to_d3d(a), to_d3d(b), to_d3d(c), area);
                if footprint > 1.0 {
                    t.min_filter == GL_LINEAR
                } else {
                    t.mag_filter == GL_LINEAR
                }
            })
    } else {
        false
    };
    for py in y0..y1 {
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
            // Perspective-correct interpolation (affine when w is constant,
            // e.g. an orthographic projection).
            let ia = if a.w > 0.0 { 1.0 / a.w } else { 1.0 };
            let ib = if b.w > 0.0 { 1.0 / b.w } else { 1.0 };
            let ic = if c.w > 0.0 { 1.0 / c.w } else { 1.0 };
            let iw = wa * ia + wb * ib + wc * ic;
            let interp = |attr_a: f32, attr_b: f32, attr_c: f32| {
                if iw != 0.0 {
                    (wa * attr_a * ia + wb * attr_b * ib + wc * attr_c * ic) / iw
                } else {
                    wa * attr_a + wb * attr_b + wc * attr_c
                }
            };
            let mut varyings = [0.0_f32; MAX_VARYING_FLOATS];
            for (slot, dst) in varyings.iter_mut().enumerate() {
                let av = a.varyings.get(slot).copied().unwrap_or(0.0);
                let bv = b.varyings.get(slot).copied().unwrap_or(0.0);
                let cv = c.varyings.get(slot).copied().unwrap_or(0.0);
                *dst = interp(av, bv, cv);
            }
            write_fragment(
                ctx,
                px,
                py,
                interp(a.z, b.z, c.z),
                interp(ar, br, cr),
                interp(ag, bg, cg),
                interp(ab, bb, cb),
                interp(aa, ba, ca),
                interp(a.u, b.u, c.u),
                interp(a.v, b.v, c.v),
                linear,
                &varyings,
            );
        }
    }
}

/// Convert to the d3d9 screen-vertex shape (the shared footprint helper).
#[must_use]
fn to_d3d(v: GlScreenVertex) -> ScreenVertex {
    ScreenVertex {
        x: v.x,
        y: v.y,
        z: v.z,
        w: v.w,
        color: v.color,
        u: v.u,
        v: v.v,
    }
}
