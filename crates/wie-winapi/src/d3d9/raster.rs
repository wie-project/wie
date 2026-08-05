use anyhow::{Context, Result};

use super::{D3DFMT_INDEX32, DepthStencilRecord, TextureRecord};
use crate::WinApiState;
use crate::d3d9_render::{
    ClipVertex, D3DPT_LINELIST, D3DPT_LINESTRIP, D3DPT_POINTLIST, D3DPT_TRIANGLEFAN,
    D3DPT_TRIANGLELIST, D3DPT_TRIANGLESTRIP, D3DRS_POINTSIZE, D3DTOP_DISABLE, FragmentState,
    FvfLayout, GuestVertex, MAX_MIP_LEVELS, Mat4, MipChain, MipLevelView, PsProgram, RenderState,
    ScreenVertex, TextureStage, TextureStageState, Viewport, VsProgram, clip_polygon_near,
    mat4_mul, parse_vertex, rasterize_line, rasterize_point, rasterize_triangle, run_vertex_shader,
    screen_from_clip, transform_point, vs_input_from_vertex,
};
use crate::d3d9_shader::{PS_SAMPLER_COUNT, ShaderKind, VS_BOOL_CONST_COUNT, VS_INT_CONST_COUNT};
use crate::gdi32::IRect;

// ── P3 software-render handlers (slice 1) ────────────────────────────────
//
// The guest renders through the D3D9 pipeline: Clear fills the host-owned
// backbuffer, Draw*(UP) rasterize triangles into it (host CPU), and Present
// publishes it through the existing PresentState surface pipeline — the same
// path GDI BitBlt uses.

/// Read one little-endian `f32` from a fixed-size byte buffer at `offset`.
pub(crate) fn read_f32_at(bytes: &[u8], offset: usize) -> f32 {
    let end = offset.saturating_add(4);
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    f32::from_le_bytes(raw)
}

/// Read one little-endian `u32` from a fixed-size byte buffer at `offset`.
pub(crate) fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    let end = offset.saturating_add(4);
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    u32::from_le_bytes(raw)
}

/// Parse a column-major `D3DMATRIX` (16 floats) from a byte buffer.
pub(crate) fn parse_mat4(bytes: &[u8; 64]) -> Mat4 {
    let mut matrix = [0.0_f32; 16];
    for (index, slot) in matrix.iter_mut().enumerate() {
        *slot = read_f32_at(bytes, index.saturating_mul(4));
    }
    matrix
}

/// Fill one clipped rect of the backbuffer with `color` (0RGB).
// Wide signature: one rect = 4 coords + dims + color is the natural rasterizer call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_backbuffer_rect(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    color: u32,
) {
    let width_i = i32::try_from(width).unwrap_or(i32::MAX);
    let height_i = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = right.min(width_i);
    let y1 = bottom.min(height_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width_us = usize::try_from(width).unwrap_or(0);
    let row_width = usize::try_from(x1.saturating_sub(x0)).unwrap_or(0);
    for y in y0..y1 {
        let start = usize::try_from(y)
            .unwrap_or(0)
            .saturating_mul(width_us)
            .saturating_add(usize::try_from(x0).unwrap_or(0));
        let end = start.saturating_add(row_width);
        if let Some(row) = backbuffer.get_mut(start..end) {
            for pixel in row {
                *pixel = color;
            }
        }
    }
}

/// Number of vertices covered by `primitive_count` primitives of `type`, or
/// `None` for a count that would overflow. Point and line primitives are
/// real since L4: a point list uses one vertex per point, a line list two per
/// line, a line strip `count + 1`.
pub(crate) fn primitive_vertex_count(primitive_type: u64, primitive_count: u64) -> Option<usize> {
    let count = usize::try_from(primitive_count & u64::from(u32::MAX)).unwrap_or(0);
    match u32::try_from(primitive_type & u64::from(u32::MAX)).unwrap_or(u32::MAX) {
        D3DPT_POINTLIST => Some(count),
        D3DPT_LINELIST => count.checked_mul(2),
        D3DPT_LINESTRIP => count.checked_add(1),
        D3DPT_TRIANGLELIST => count.checked_mul(3),
        D3DPT_TRIANGLESTRIP | D3DPT_TRIANGLEFAN => count.checked_add(2),
        _ => Some(0),
    }
}

/// The per-primitive vertex-index groups of a draw, in stream order
/// (pre-indexing — the indexed form resolves each through the index buffer).
#[derive(Debug)]
pub(crate) enum PrimitiveGroups {
    /// Point list: one vertex per point.
    Points(Vec<usize>),
    /// Line list/strip: one vertex pair per segment.
    Lines(Vec<(usize, usize)>),
    /// Triangle list/strip/fan: one triple per triangle.
    Triangles(Vec<(usize, usize, usize)>),
}

impl PrimitiveGroups {
    /// Whether the draw produced no primitives (the caller skips it).
    fn is_empty(&self) -> bool {
        match self {
            Self::Points(v) => v.is_empty(),
            Self::Lines(v) => v.is_empty(),
            Self::Triangles(v) => v.is_empty(),
        }
    }
}

fn primitive_groups(primitive_type: u64, primitive_count: u64) -> Result<PrimitiveGroups> {
    let count = usize::try_from(primitive_count & u64::from(u32::MAX))
        .context("primitive count does not fit usize")?;
    match u32::try_from(primitive_type & u64::from(u32::MAX)).unwrap_or(u32::MAX) {
        D3DPT_POINTLIST => Ok(PrimitiveGroups::Points((0..count).collect())),
        D3DPT_LINELIST => Ok(PrimitiveGroups::Lines(
            (0..count)
                .map(|i| {
                    let base = i.saturating_mul(2);
                    (base, base.saturating_add(1))
                })
                .collect(),
        )),
        D3DPT_LINESTRIP => Ok(PrimitiveGroups::Lines(
            (0..count).map(|i| (i, i.saturating_add(1))).collect(),
        )),
        D3DPT_TRIANGLELIST => Ok(PrimitiveGroups::Triangles(
            (0..count)
                .map(|i| {
                    let base = i.saturating_mul(3);
                    (base, base.saturating_add(1), base.saturating_add(2))
                })
                .collect(),
        )),
        D3DPT_TRIANGLESTRIP => Ok(PrimitiveGroups::Triangles(
            (0..count)
                .map(|i| (i, i.saturating_add(1), i.saturating_add(2)))
                .collect(),
        )),
        D3DPT_TRIANGLEFAN => Ok(PrimitiveGroups::Triangles(
            (0..count)
                .map(|i| (0, i.saturating_add(1), i.saturating_add(2)))
                .collect(),
        )),
        // Unknown primitive types draw nothing.
        _ => Ok(PrimitiveGroups::Points(Vec::new())),
    }
}

/// Read index `n` from a raw index buffer (`size` = 2 for INDEX16, 4 for
/// INDEX32).
fn read_index(bytes: &[u8], n: usize, size: usize) -> Option<usize> {
    let start = n.checked_mul(size)?;
    let end = start.checked_add(size)?;
    let raw = bytes.get(start..end)?;
    if size == 4 {
        let word: [u8; 4] = raw.try_into().ok()?;
        usize::try_from(u32::from_le_bytes(word)).ok()
    } else {
        let half: [u8; 2] = raw.try_into().ok()?;
        Some(usize::from(u16::from_le_bytes(half)))
    }
}

/// Resolve one triangle corner to a vertex: either the stream position
/// directly (non-indexed) or through the index buffer (indexed).
///
/// `indices` is `(bytes, index size, start-index offset)` — the offset is
/// `DrawIndexedPrimitive`'s `StartIndex`, the first buffer position used.
/// `vertex_base` is `BaseVertexIndex`, added to every resolved index (a
/// negative base references vertices before the indexed window; out-of-range
/// results reject the vertex).
fn indexed_vertex(
    data: &[u8],
    layout: &FvfLayout,
    stride: usize,
    indices: Option<(&[u8], usize, usize)>,
    vertex_base: i64,
    vertex_index: usize,
) -> Option<GuestVertex> {
    let index = match indices {
        Some((bytes, size, offset)) => read_index(bytes, offset.checked_add(vertex_index)?, size)?,
        None => vertex_index,
    };
    let resolved = i64::try_from(index).ok()?.checked_add(vertex_base)?;
    let resolved = usize::try_from(resolved).ok()?;
    parse_vertex(data, resolved.checked_mul(stride)?, layout)
}

/// Build the per-draw blend + depth fragment state from the typed device
/// render state.
///
/// Borrows the depth buffer (when bound) through a field-level mutable borrow
/// so the caller can hold the backbuffer mutably at the same time. `scissor`
/// is the `SetScissorRect` rect (a device-level state, not a `D3DRS_*`).
fn build_fragment_state<'a>(
    render_state: &RenderState,
    depth_stencil: u64,
    depth_surfaces: &'a mut ahash::HashMap<u64, DepthStencilRecord>,
    scissor: Option<IRect>,
) -> FragmentState<'a> {
    let depth = if depth_stencil != 0 {
        depth_surfaces
            .get_mut(&depth_stencil)
            .map(|record| record.depth.as_mut_slice())
    } else {
        None
    };
    FragmentState {
        depth,
        z_enable: render_state.z_enable.as_u32(),
        z_func: render_state.z_func.as_u32(),
        z_write: u32::from(render_state.z_write_enable),
        alpha_blend: u32::from(render_state.alpha_blend_enable),
        src_blend: render_state.src_blend.as_u32(),
        dest_blend: render_state.dest_blend.as_u32(),
        blend_op: render_state.blend_op.as_u32(),
        fog_enable: u32::from(render_state.fog_enable),
        fog_color: render_state.fog_color,
        fog_start: render_state.fog_start,
        fog_end: render_state.fog_end,
        fog_density: render_state.fog_density,
        fog_table_mode: render_state.fog_table_mode,
        fog_vertex_mode: render_state.fog_vertex_mode,
        alpha_test: u32::from(render_state.alpha_test_enable),
        alpha_func: render_state.alpha_func.as_u32(),
        alpha_ref: render_state.alpha_ref,
        scissor_test: u32::from(render_state.scissor_test_enable),
        scissor,
    }
}

/// Resolve one texture stage's sampling state, or `None` when no texture is
/// bound or the binding is stale.
///
/// Borrows the state slices directly (field-level) so the caller can hold the
/// backbuffer mutably at the same time. The per-stage struct merges the TSS
/// and sampler namespaces (their constants collide numerically, so each
/// `Set*` call routes to its own field — see [`TextureStageState`]).
fn resolve_sampler_stage<'a>(
    stage_idx: usize,
    stages: &[TextureStageState; 8],
    bindings: &[u64; 8],
    textures: &'a ahash::HashMap<u64, TextureRecord>,
) -> Option<TextureStage<'a>> {
    let binding = bindings.get(stage_idx).copied().unwrap_or(0);
    if binding == 0 {
        return None;
    }
    let record = textures.get(&binding)?;
    if record.width == 0 || record.height == 0 || record.pixels.is_empty() {
        return None;
    }
    let stage = stages.get(stage_idx)?;
    // L4 mip chain: level 0 is the record's full-res texels; levels 1..
    // are the halved per-level buffers from the CreateTexture desc.
    let mut chain = MipChain {
        count: 0,
        levels: [None; MAX_MIP_LEVELS],
    };
    if let Some(slot) = chain.levels.first_mut() {
        *slot = Some(MipLevelView {
            width: record.width,
            height: record.height,
            pixels: &record.pixels,
        });
    }
    chain.count = record.levels;
    for (index, mip) in record.mip_levels.iter().enumerate() {
        if let Some(slot) = chain.levels.get_mut(index.saturating_add(1)) {
            *slot = Some(MipLevelView {
                width: mip.width,
                height: mip.height,
                pixels: &mip.pixels,
            });
        }
    }
    Some(TextureStage {
        pixels: &record.pixels,
        width: record.width,
        height: record.height,
        addr_u: stage.address_u,
        addr_v: stage.address_v,
        mag_filter: stage.mag_filter,
        min_filter: stage.min_filter,
        mip_filter: stage.mip_filter,
        mips: chain,
        color_op: stage.color_op,
        color_arg1: stage.color_arg1,
        color_arg2: stage.color_arg2,
        alpha_op: stage.alpha_op,
        alpha_arg1: stage.alpha_arg1,
        alpha_arg2: stage.alpha_arg2,
    })
}

/// Resolve the stage-0 texture sampling state for the FFP path, or `None`
/// when no texture is bound, the stage is disabled, or the binding is stale.
fn resolve_texture_stage<'a>(
    stages: &[TextureStageState; 8],
    bindings: &[u64; 8],
    textures: &'a ahash::HashMap<u64, TextureRecord>,
) -> Option<TextureStage<'a>> {
    let stage = resolve_sampler_stage(0, stages, bindings, textures)?;
    // The per-stage struct carries D3D9's stage-0 FFP defaults, so unset
    // fields already read as the legacy fallback values.
    if stage.color_op == D3DTOP_DISABLE {
        return None;
    }
    Some(stage)
}

/// Resolve the `s0..s3` sampler registers for a bound pixel shader.
///
/// When a pixel shader is bound, the FFP color/alpha ops are ignored — the
/// sampler stage state (address modes, filter) alone drives `texld`.
fn resolve_ps_samplers<'a>(
    stages: &[TextureStageState; 8],
    bindings: &[u64; 8],
    textures: &'a ahash::HashMap<u64, TextureRecord>,
) -> [Option<TextureStage<'a>>; PS_SAMPLER_COUNT] {
    let mut samplers = [None; PS_SAMPLER_COUNT];
    for (index, slot) in samplers.iter_mut().enumerate() {
        *slot = resolve_sampler_stage(index, stages, bindings, textures);
    }
    samplers
}

/// Resolve a bound vertex shader into an executable program: the parsed
/// instructions (borrowed from the shader record) and a copy of the float
/// constant registers (`SetVertexShaderConstantF` + `def` from Create time).
/// The int/bool constant files are the zeroed defaults — the L5 handlers that
/// set them (`SetVertexShaderConstantI/B`) land in parallel and will wire the
/// real registers here. `None` when no shader is bound — the FFP transform
/// runs.
///
/// Takes the shader-record map by field-level reference so the returned
/// program borrows only that field (the caller holds the backbuffer mutably
/// below).
fn resolve_vs_program<'a>(
    current: u64,
    shaders: &'a ahash::HashMap<u64, crate::d3d9_shader::ShaderRecord>,
    constants: [[f32; 4]; crate::d3d9_shader::VS_CONST_COUNT],
    int_constants: [[i32; 4]; VS_INT_CONST_COUNT],
    bool_constants: [bool; VS_BOOL_CONST_COUNT],
) -> Option<VsProgram<'a>> {
    if current == 0 {
        return None;
    }
    shaders.get(&current).and_then(|record| {
        (record.kind == ShaderKind::Vertex).then(|| VsProgram {
            instructions: &record.parsed.instructions,
            constants,
            int_constants,
            bool_constants,
        })
    })
}

/// Run the bound vertex shader on one FVF-decoded vertex and return its
/// clip-space output with the interpolated attributes (the caller clips and
/// viewport-transforms).
fn vs_vertex_to_clip(program: &VsProgram<'_>, v: &GuestVertex, layout: &FvfLayout) -> ClipVertex {
    let input = vs_input_from_vertex(v, layout);
    let out = run_vertex_shader(program, &input);
    ClipVertex {
        pos: out.pos,
        color: out.color,
        u: out.u,
        v: out.v,
    }
}

/// Transform one FVF-decoded vertex to its draw-space form.
///
/// The vertex shader output and the FFP-transformed `XYZ` vertex are
/// clip-space (near-plane clipped per primitive); `XYZRHW` is already
/// screen-space (z viewport-mapped, no clip — negative RHW is undefined).
enum TransformedVertex {
    /// Already screen-space (`XYZRHW`): rasterize directly.
    Screen(ScreenVertex),
    /// Clip-space (VS output or FFP `XYZ`): near-clip then viewport-map.
    Clip(ClipVertex),
}

fn transform_vertex(
    v: &GuestVertex,
    pre_transformed: bool,
    matrix: &Mat4,
    viewport: &Viewport,
    vs_program: Option<&VsProgram<'_>>,
    layout: &FvfLayout,
) -> TransformedVertex {
    if let Some(program) = vs_program {
        TransformedVertex::Clip(vs_vertex_to_clip(program, v, layout))
    } else if pre_transformed {
        TransformedVertex::Screen(ScreenVertex {
            x: v.x,
            y: v.y,
            z: viewport.min_z + v.z.clamp(0.0, 1.0) * (viewport.max_z - viewport.min_z),
            w: 1.0,
            color: v.color,
            u: v.u,
            v: v.v,
        })
    } else {
        TransformedVertex::Clip(ClipVertex {
            pos: transform_point([v.x, v.y, v.z, v.w], matrix),
            color: v.color,
            u: v.u,
            v: v.v,
        })
    }
}

/// Map a clip-space vertex to screen (`None` only for the defensive
/// `w <= 0` guard — the near-plane clip keeps `w > 0`).
fn clip_to_screen_vertex(cv: &ClipVertex, viewport: &Viewport) -> Option<ScreenVertex> {
    screen_from_clip(cv, viewport)
}

/// Rasterize a batched vertex stream into the backbuffer.
///
/// `data` is the full vertex pool; `groups` names each primitive; `indices`
/// (when present) resolves primitive corners through an index buffer (`bytes`,
/// index size, and the `StartIndex` offset into it). `vertex_base` is added
/// to every resolved index (`BaseVertexIndex`). When a vertex shader is bound,
/// each vertex runs through the VS interpreter (FVF decode → `v0..v15` →
/// `oPos`); otherwise the world × view × projection transform applies.
/// Triangles/segments straddling the near plane are clipped
/// (Sutherland–Hodgman) instead of rejected; points behind it are skipped.
/// Accumulates the dirty region, and the fragment stage samples a bound
/// stage-0 texture.
fn rasterize_vertex_stream(
    state: &mut WinApiState,
    data: &[u8],
    layout: &FvfLayout,
    stride: usize,
    groups: &PrimitiveGroups,
    indices: Option<(&[u8], usize, usize)>,
    vertex_base: i64,
) {
    let (width, height) = (
        state.d3d9().d3d9_backbuffer_width,
        state.d3d9().d3d9_backbuffer_height,
    );
    if width == 0 || height == 0 || groups.is_empty() {
        return;
    }
    let d3d = state.d3d9();
    // Copy the transform state out of the borrowed state (small f32 copies)
    // so the rasterizer can hold the backbuffer mutably below.
    let world = d3d.d3d9_world_matrix;
    let view = d3d.d3d9_view_matrix;
    let projection = d3d.d3d9_projection_matrix;
    let (vp_x, vp_y, vp_w, vp_h, vp_min_z, vp_max_z) = d3d.d3d9_viewport;
    let pre_transformed = layout.pre_transformed;
    let mut dirty = d3d.d3d9_dirty;
    let matrix = mat4_mul(&world, &mat4_mul(&view, &projection));
    let viewport = Viewport {
        x: vp_x,
        y: vp_y,
        width: vp_w,
        height: vp_h,
        min_z: vp_min_z,
        max_z: vp_max_z,
    };
    // L4 point size: `D3DRS_POINTSIZE` (a float) rides the raw-value render
    // state layer (unmodeled states round-trip verbatim); default 1.0.
    let point_size = f32::from_bits(
        d3d.d3d9_render_state_raw
            .get(&D3DRS_POINTSIZE)
            .copied()
            .unwrap_or(0x3F80_0000),
    );
    // Resolve the texture stage through field-level borrows (the backbuffer
    // is held mutably below, so the stage must not borrow the whole struct).
    let tex = resolve_texture_stage(
        &d3d.d3d9_stage_states,
        &d3d.d3d9_texture_bindings,
        &d3d.d3d9_textures,
    );
    // Resolve a bound pixel shader into an executable program: the parsed
    // instructions (borrowed from the shader record), a copy of the constant
    // registers (SetPixelShaderConstantF + def from Create time), and the
    // s0..s3 sampler stages.
    let ps_samplers = resolve_ps_samplers(
        &d3d.d3d9_stage_states,
        &d3d.d3d9_texture_bindings,
        &d3d.d3d9_textures,
    );
    let ps = if d3d.d3d9_pixel_shader == 0 {
        None
    } else {
        d3d.d3d9_shaders
            .get(&d3d.d3d9_pixel_shader)
            .and_then(|record| {
                (record.kind == ShaderKind::Pixel).then(|| {
                    let sampler_refs: [Option<&TextureStage<'_>>; PS_SAMPLER_COUNT] =
                        std::array::from_fn(|index| {
                            ps_samplers.get(index).and_then(|s| s.as_ref())
                        });
                    PsProgram {
                        instructions: &record.parsed.instructions,
                        constants: d3d.d3d9_ps_constants,
                        samplers: sampler_refs,
                    }
                })
            })
    };
    // A bound vertex shader replaces the FFP transform: each vertex runs the
    // interpreter and its oPos feeds the clip/near-clip path directly (the
    // world/view/projection matrices are ignored in the programmable path).
    let vs_program = resolve_vs_program(
        d3d.d3d9_current_vertex_shader,
        &d3d.d3d9_shaders,
        d3d.d3d9_vs_constants,
        d3d.d3d9_vs_int_constants,
        d3d.d3d9_vs_bool_constants,
    );
    // Resolve the blend + depth fragment state (mutably borrows the bound
    // depth buffer — a different field than the backbuffer).
    let mut frag = build_fragment_state(
        &d3d.d3d9_render_state,
        d3d.d3d9_depth_stencil,
        &mut d3d.d3d9_depth_surfaces,
        d3d.d3d9_scissor_rect,
    );
    let transform = |v: GuestVertex| -> TransformedVertex {
        transform_vertex(
            &v,
            pre_transformed,
            &matrix,
            &viewport,
            vs_program.as_ref(),
            layout,
        )
    };
    match groups {
        PrimitiveGroups::Points(points) => {
            for &i0 in points {
                let Some(v0) = indexed_vertex(data, layout, stride, indices, vertex_base, i0)
                else {
                    continue;
                };
                match transform(v0) {
                    TransformedVertex::Screen(sv) => rasterize_point(
                        &mut d3d.d3d9_backbuffer,
                        width,
                        height,
                        sv,
                        point_size,
                        tex.as_ref(),
                        ps.as_ref(),
                        &mut frag,
                        &mut dirty,
                    ),
                    // A point is either fully in front (draw) or behind
                    // (skip) — clipping a point would only drop it.
                    TransformedVertex::Clip(cv) => {
                        if let Some(sv) = clip_to_screen_vertex(&cv, &viewport) {
                            rasterize_point(
                                &mut d3d.d3d9_backbuffer,
                                width,
                                height,
                                sv,
                                point_size,
                                tex.as_ref(),
                                ps.as_ref(),
                                &mut frag,
                                &mut dirty,
                            );
                        }
                    }
                }
            }
        }
        PrimitiveGroups::Lines(lines) => {
            for &(i0, i1) in lines {
                let (Some(v0), Some(v1)) = (
                    indexed_vertex(data, layout, stride, indices, vertex_base, i0),
                    indexed_vertex(data, layout, stride, indices, vertex_base, i1),
                ) else {
                    continue;
                };
                match (transform(v0), transform(v1)) {
                    (TransformedVertex::Screen(a), TransformedVertex::Screen(b)) => {
                        rasterize_line(
                            &mut d3d.d3d9_backbuffer,
                            width,
                            height,
                            a,
                            b,
                            tex.as_ref(),
                            ps.as_ref(),
                            &mut frag,
                            &mut dirty,
                        );
                    }
                    (TransformedVertex::Clip(a), TransformedVertex::Clip(b)) => {
                        // Near-clip the segment: 0 (fully behind), 1
                        // (touches the plane), or 2 (straddles) vertices.
                        let clipped = clip_polygon_near(&[a, b]);
                        if clipped.len() == 2 {
                            let (Some(a), Some(b)) = (
                                clip_to_screen_vertex(&clipped[0], &viewport),
                                clip_to_screen_vertex(&clipped[1], &viewport),
                            ) else {
                                continue;
                            };
                            rasterize_line(
                                &mut d3d.d3d9_backbuffer,
                                width,
                                height,
                                a,
                                b,
                                tex.as_ref(),
                                ps.as_ref(),
                                &mut frag,
                                &mut dirty,
                            );
                        }
                    }
                    // Mixed screen/clip forms are impossible (the transform
                    // is uniform per draw) — skip defensively.
                    _ => {}
                }
            }
        }
        PrimitiveGroups::Triangles(triples) => {
            for &(i0, i1, i2) in triples {
                let (Some(v0), Some(v1), Some(v2)) = (
                    indexed_vertex(data, layout, stride, indices, vertex_base, i0),
                    indexed_vertex(data, layout, stride, indices, vertex_base, i1),
                    indexed_vertex(data, layout, stride, indices, vertex_base, i2),
                ) else {
                    continue;
                };
                match (transform(v0), transform(v1), transform(v2)) {
                    (
                        TransformedVertex::Screen(a),
                        TransformedVertex::Screen(b),
                        TransformedVertex::Screen(c),
                    ) => {
                        rasterize_triangle(
                            &mut d3d.d3d9_backbuffer,
                            width,
                            height,
                            a,
                            b,
                            c,
                            tex.as_ref(),
                            ps.as_ref(),
                            &mut frag,
                            &mut dirty,
                        );
                    }
                    (
                        TransformedVertex::Clip(a),
                        TransformedVertex::Clip(b),
                        TransformedVertex::Clip(c),
                    ) => {
                        // Near-clip the triangle (Sutherland–Hodgman): a
                        // straddling triangle fans into 3..4 screen triangles.
                        let clipped = clip_polygon_near(&[a, b, c]);
                        if let Some(first) = clipped.first() {
                            for pair in clipped.get(1..).unwrap_or(&[]).windows(2) {
                                let (Some(a), Some(b), Some(c)) = (
                                    clip_to_screen_vertex(first, &viewport),
                                    clip_to_screen_vertex(&pair[0], &viewport),
                                    clip_to_screen_vertex(&pair[1], &viewport),
                                ) else {
                                    continue;
                                };
                                rasterize_triangle(
                                    &mut d3d.d3d9_backbuffer,
                                    width,
                                    height,
                                    a,
                                    b,
                                    c,
                                    tex.as_ref(),
                                    ps.as_ref(),
                                    &mut frag,
                                    &mut dirty,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    d3d.d3d9_dirty = dirty;
}

/// Batched-memory-read a guest vertex pool (+ optional guest index buffer)
/// and rasterize — the Draw*UP path.
///
/// The guest vertex data is read with ONE `mem_read` per buffer (the GDI blit
/// span pattern), then parsed host-side by FVF layout. Unreadable/malformed
/// buffers are skipped (return `Ok(())`) — a bad pointer must not crash the
/// guest; the draw simply produces no pixels.
// Wide signature: one full draw command (stream + FVF + primitive + indices).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_vertex_stream(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    data_ptr: u64,
    layout: &FvfLayout,
    stride: usize,
    vertex_count: usize,
    primitive_type: u64,
    primitive_count: u64,
    index_ptr: u64,
    index_format: u32,
    index_count: usize,
) -> Result<()> {
    if vertex_count == 0 || stride == 0 || data_ptr == 0 {
        return Ok(());
    }
    let groups = primitive_groups(primitive_type, primitive_count)?;
    if groups.is_empty() {
        return Ok(());
    }

    let data_bytes = vertex_count
        .checked_mul(stride)
        .context("vertex stream size overflow")?;
    let mut data = vec![0_u8; data_bytes];
    if engine.mem_read(data_ptr, &mut data).is_err() {
        // Unmapped guest memory: skip the draw rather than fault the guest.
        return Ok(());
    }

    let indices = if index_count > 0 && index_ptr != 0 {
        // D3DFMT_INDEX32 = 102 (4-byte indices); everything else — including
        // D3DFMT_INDEX16 = 101 — is 16-bit. Unknown formats default lenient.
        let size = usize::try_from(if index_format == D3DFMT_INDEX32 { 4 } else { 2 })
            .context("index size does not fit usize")?;
        let index_bytes = index_count
            .checked_mul(size)
            .context("index buffer size overflow")?;
        let mut index_data = vec![0_u8; index_bytes];
        if engine.mem_read(index_ptr, &mut index_data).is_err() {
            return Ok(());
        }
        Some((index_data, size))
    } else {
        None
    };

    draw_vertex_stream_host(
        state,
        &data,
        layout,
        stride,
        vertex_count,
        primitive_type,
        primitive_count,
        indices
            .as_ref()
            .map(|(bytes, size)| (bytes.as_slice(), *size)),
        0,
        0,
    )
}
/// Rasterize a host-side vertex pool (+ optional host index data) — the
/// buffer-form draw path (`DrawPrimitive`/`DrawIndexedPrimitive`).
///
/// `data` starts at the stream base already (the `SetStreamSource`
/// `OffsetInBytes` and `DrawPrimitive`'s `StartVertex` are baked into the
/// slice by the caller). `indices` carries `(bytes, index size)`; `index_offset`
/// is `StartIndex`; `vertex_base` is `BaseVertexIndex`. Out-of-range vertices
/// reject their triangle (the same skip the UP path uses for bad pointers) —
/// no guest memory is touched.
// Wide signature: one full draw command (host stream + FVF + primitive + indices).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_vertex_stream_host(
    state: &mut WinApiState,
    data: &[u8],
    layout: &FvfLayout,
    stride: usize,
    _vertex_count: usize,
    primitive_type: u64,
    primitive_count: u64,
    indices: Option<(&[u8], usize)>,
    index_offset: usize,
    vertex_base: i64,
) -> Result<()> {
    if data.is_empty() || stride == 0 {
        return Ok(());
    }
    let groups = primitive_groups(primitive_type, primitive_count)?;
    if groups.is_empty() {
        return Ok(());
    }
    rasterize_vertex_stream(
        state,
        data,
        layout,
        stride,
        &groups,
        indices.map(|(bytes, size)| (bytes, size, index_offset)),
        vertex_base,
    );
    Ok(())
}
