// ── Fragment stage (depth test, texture, blend) ─────────────────────────
//
// The per-pixel pipeline shared by the point / line / triangle rasterizers
// (in the parent module): bounds → depth test+write → texture sample and env
// combine → alpha blend → backbuffer store.

use super::*;
use crate::d3d9_render::is_top_or_left_edge;

/// GL depth compare against `func` (default `GL_LESS`).
#[must_use]
fn depth_pass(z: f32, existing: f32, func: u32) -> bool {
    match func {
        GL_NEVER => false,
        GL_EQUAL => z == existing,
        GL_NOTEQUAL => z != existing,
        GL_LESS => z < existing,
        GL_LEQUAL => z <= existing,
        GL_GREATER => z > existing,
        GL_GEQUAL => z >= existing,
        // GL_ALWAYS and unknown funcs always pass.
        _ => true,
    }
}

/// Resolve a `GL_*` blend factor to a 0..1 fraction.
#[must_use]
fn blend_factor(factor: u32, src: f32, dst: f32, src_alpha: f32, dst_alpha: f32) -> f32 {
    match factor {
        GL_ZERO => 0.0,
        GL_SRC_COLOR => src,
        GL_ONE_MINUS_SRC_COLOR => 1.0 - src,
        GL_SRC_ALPHA => src_alpha,
        GL_ONE_MINUS_SRC_ALPHA => 1.0 - src_alpha,
        GL_DST_ALPHA => dst_alpha,
        GL_ONE_MINUS_DST_ALPHA => 1.0 - dst_alpha,
        GL_DST_COLOR => dst,
        GL_ONE_MINUS_DST_COLOR => 1.0 - dst,
        // GL_ONE and unknown factors.
        _ => 1.0,
    }
}

/// Blend a fragment color over the stored destination: `out = src·sf + dst·df`
/// per channel (GL non-premultiplied semantics), keeping the alpha channel.
#[must_use]
fn blend_fragment_gl(
    dst: u32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    src_factor: u32,
    dst_factor: u32,
) -> u32 {
    let [dr, dg, db, da] = unpack_color(dst);
    let channel = |src: f32, d: f32| {
        let sf = blend_factor(src_factor, src, d, a, da);
        let df = blend_factor(dst_factor, src, d, a, da);
        (src * sf + d * df).clamp(0.0, 1.0)
    };
    pack_color([
        channel(r, dr),
        channel(g, dg),
        channel(b, db),
        channel(a, da),
    ])
}

/// Write one fragment: bounds check → depth test+write → fragment shader or
/// fixed-function texture sample+env → alpha blend → backbuffer store.
// Wide signature: one fragment carries position, depth, color, uv, and the
// interpolated shader varyings.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_fragment(
    ctx: &mut GlCtx,
    px: i32,
    py: i32,
    z: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    u: f32,
    v: f32,
    linear: bool,
    varyings: &[f32],
) {
    if px < 0 || py < 0 {
        return;
    }
    let w_i = i32::try_from(ctx.width).unwrap_or(0);
    let h_i = i32::try_from(ctx.height).unwrap_or(0);
    if px >= w_i || py >= h_i {
        return;
    }
    let index = usize::try_from(py)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(ctx.width).unwrap_or(0))
        .saturating_add(usize::try_from(px).unwrap_or(0));
    let pass = if ctx.depth_test {
        let existing = ctx.depth.get(index).copied().unwrap_or(1.0);
        depth_pass(z, existing, ctx.depth_func)
    } else {
        true
    };
    if !pass {
        return;
    }
    if ctx.depth_mask
        && let Some(slot) = ctx.depth.get_mut(index)
    {
        *slot = z;
    }
    // Field-level borrows: the shader/texture lookups borrow `programs` /
    // `textures` while the writes borrow `backbuffer` — disjoint fields.
    let (fr, fg, fb, fa) =
        if let Some(color) = run_active_fragment_shader(ctx, px, py, z, u, v, varyings) {
            (
                color.first().copied().unwrap_or(0.0),
                color.get(1).copied().unwrap_or(0.0),
                color.get(2).copied().unwrap_or(0.0),
                color.get(3).copied().unwrap_or(1.0),
            )
        } else {
            let tex = if ctx.texture_2d {
                ctx.textures
                    .iter()
                    .find(|t| t.id == ctx.bound_texture && !t.pixels.is_empty())
            } else {
                None
            };
            if let Some(tex) = tex {
                let texel = sample_texel(tex, u, v, linear);
                match ctx.tex_env {
                    GL_REPLACE => (texel[0], texel[1], texel[2], texel[3]),
                    // GL_MODULATE and unknown env modes.
                    _ => (r * texel[0], g * texel[1], b * texel[2], a * texel[3]),
                }
            } else {
                (r, g, b, a)
            }
        };
    let color = if ctx.blend {
        let dst = ctx.backbuffer.get(index).copied().unwrap_or(0);
        blend_fragment_gl(dst, fr, fg, fb, fa, ctx.src_blend, ctx.dst_blend)
    } else {
        pack_color([fr, fg, fb, fa])
    };
    if let Some(pixel) = ctx.backbuffer.get_mut(index) {
        *pixel = color;
    }
}

/// Run the active program's fragment shader on this pixel; `None` when no
/// linked FS is active (the fixed-function path runs instead).
fn run_active_fragment_shader(
    ctx: &GlCtx,
    px: i32,
    py: i32,
    z: f32,
    _u: f32,
    _v: f32,
    varyings: &[f32],
) -> Option<[f32; 4]> {
    let program = ctx.program_active;
    if program == 0 {
        return None;
    }
    // Cached index from gl_use_program; avoids a per-pixel linear scan.
    let prog = ctx
        .active_program_idx
        .and_then(|idx| ctx.programs.get(idx))
        .filter(|p| p.id == program)?;
    let fs = prog.fs.as_ref()?;
    let exec = super::glsl_exec::ProgramExec {
        uniforms: &prog.uniforms,
        uniform_index: &prog.uniform_index,
    };
    let out = super::glsl_exec::run_fragment_shader(
        &exec,
        fs,
        super::glsl_exec::FsInputs {
            frag_coord: [px as f32 + 0.5, py as f32 + 0.5, z, 1.0],
            varyings,
            textures: &ctx.textures,
            texture_index: &ctx.texture_index,
            texture_units: &ctx.texture_unit_bindings,
        },
    );
    Some(out.color)
}

/// Top-left boundary rule (shared edges owned by exactly one triangle).
#[must_use]
pub(super) fn edge_inside(e: f32, dx: f32, dy: f32) -> bool {
    e > 0.0 || (e == 0.0 && is_top_or_left_edge(dx, dy))
}
