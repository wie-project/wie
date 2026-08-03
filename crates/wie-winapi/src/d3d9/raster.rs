use anyhow::{Context, Result};

use super::{D3DFMT_INDEX32, DepthStencilRecord, TextureRecord};
use crate::WinApiState;
use crate::d3d9_render::{
    D3DPT_TRIANGLEFAN, D3DPT_TRIANGLELIST, D3DPT_TRIANGLESTRIP, D3DTOP_DISABLE, FragmentState,
    FvfLayout, GuestVertex, Mat4, PsProgram, RenderState, TextureStage, TextureStageState,
    Viewport, draw_triangle, mat4_mul, parse_vertex,
};
use crate::d3d9_shader::{PS_SAMPLER_COUNT, ShaderKind};

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
/// `None` for a count that would overflow. Unsupported primitive types (point
/// and line lists) return `Some(0)` — nothing to draw.
pub(crate) fn primitive_vertex_count(primitive_type: u64, primitive_count: u64) -> Option<usize> {
    let count = usize::try_from(primitive_count & u64::from(u32::MAX)).unwrap_or(0);
    match u32::try_from(primitive_type & u64::from(u32::MAX)).unwrap_or(u32::MAX) {
        D3DPT_TRIANGLELIST => count.checked_mul(3),
        D3DPT_TRIANGLESTRIP | D3DPT_TRIANGLEFAN => count.checked_add(2),
        _ => Some(0),
    }
}

/// The three vertex indices of every triangle in the primitive, in stream
/// order (pre-indexing — the indexed form resolves each through the index
/// buffer).
fn triangle_index_triples(
    primitive_type: u64,
    primitive_count: u64,
) -> Result<Vec<(usize, usize, usize)>> {
    let count = usize::try_from(primitive_count & u64::from(u32::MAX))
        .context("primitive count does not fit usize")?;
    let mut triples = Vec::new();
    match u32::try_from(primitive_type & u64::from(u32::MAX)).unwrap_or(u32::MAX) {
        D3DPT_TRIANGLELIST => {
            triples.extend((0..count).map(|i| {
                let base = i.saturating_mul(3);
                (base, base.saturating_add(1), base.saturating_add(2))
            }));
        }
        D3DPT_TRIANGLESTRIP => {
            triples.extend((0..count).map(|i| (i, i.saturating_add(1), i.saturating_add(2))));
        }
        D3DPT_TRIANGLEFAN => {
            triples.extend((0..count).map(|i| (0, i.saturating_add(1), i.saturating_add(2))));
        }
        // Point/line primitives are unsupported in slice 1 — no triangles.
        _ => {}
    }
    Ok(triples)
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
fn indexed_vertex(
    data: &[u8],
    layout: &FvfLayout,
    stride: usize,
    indices: Option<(&[u8], usize)>,
    vertex_index: usize,
) -> Option<GuestVertex> {
    let index = match indices {
        Some((bytes, size)) => read_index(bytes, vertex_index, size)?,
        None => vertex_index,
    };
    parse_vertex(data, index.checked_mul(stride)?, layout)
}

/// Build the per-draw blend + depth fragment state from the typed device
/// render state.
///
/// Borrows the depth buffer (when bound) through a field-level mutable borrow
/// so the caller can hold the backbuffer mutably at the same time.
fn build_fragment_state<'a>(
    render_state: &RenderState,
    depth_stencil: u64,
    depth_surfaces: &'a mut ahash::HashMap<u64, DepthStencilRecord>,
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
    Some(TextureStage {
        pixels: &record.pixels,
        width: record.width,
        height: record.height,
        addr_u: stage.address_u,
        addr_v: stage.address_v,
        mag_filter: stage.mag_filter,
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

/// Rasterize a batched vertex stream into the backbuffer.
///
/// `data` is the full vertex pool; `triples` names each triangle; `indices`
/// (when present) resolves triangle corners through an index buffer. Reads
/// the world × view × projection transform and viewport from device state,
/// rejects triangles behind the near-plane, and accumulates the dirty region.
/// When a stage-0 texture is bound and enabled, the fragment stage samples it.
fn rasterize_vertex_stream(
    state: &mut WinApiState,
    data: &[u8],
    layout: &FvfLayout,
    stride: usize,
    triples: &[(usize, usize, usize)],
    indices: Option<(&[u8], usize)>,
) {
    let (width, height) = (
        state.d3d9().d3d9_backbuffer_width,
        state.d3d9().d3d9_backbuffer_height,
    );
    if width == 0 || height == 0 || triples.is_empty() {
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
    // Resolve the blend + depth fragment state (mutably borrows the bound
    // depth buffer — a different field than the backbuffer).
    let mut frag = build_fragment_state(
        &d3d.d3d9_render_state,
        d3d.d3d9_depth_stencil,
        &mut d3d.d3d9_depth_surfaces,
    );
    for &(i0, i1, i2) in triples {
        let Some(v0) = indexed_vertex(data, layout, stride, indices, i0) else {
            continue;
        };
        let Some(v1) = indexed_vertex(data, layout, stride, indices, i1) else {
            continue;
        };
        let Some(v2) = indexed_vertex(data, layout, stride, indices, i2) else {
            continue;
        };
        draw_triangle(
            &mut d3d.d3d9_backbuffer,
            width,
            height,
            v0,
            v1,
            v2,
            pre_transformed,
            &matrix,
            &viewport,
            tex.as_ref(),
            ps.as_ref(),
            &mut frag,
            &mut dirty,
        );
    }
    d3d.d3d9_dirty = dirty;
}

/// Batched-memory-read a vertex pool (+ optional index buffer) and rasterize.
///
/// The guest vertex data is read with ONE `mem_read` per buffer (the GDI blit
/// span pattern), then parsed host-side by FVF layout. Unreadable/malformed
/// buffers are skipped (return `Ok(())`) — a bad pointer must not crash the
/// guest; the draw simply produces no pixels.
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
    let triples = triangle_index_triples(primitive_type, primitive_count)?;
    if triples.is_empty() {
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

    rasterize_vertex_stream(
        state,
        &data,
        layout,
        stride,
        &triples,
        indices
            .as_ref()
            .map(|(bytes, size)| (bytes.as_slice(), *size)),
    );
    Ok(())
}
