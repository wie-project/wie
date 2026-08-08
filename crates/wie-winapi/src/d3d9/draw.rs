//! Shared bodies of the `IDirect3DDevice9` Draw forms: `DrawPrimitive[Indexed][UP]`
//! all funnel through the two common routines here (split from `device.rs` —
//! the draw-form dispatch handlers stay in the parent).

use anyhow::{Context, Result};

use super::buffer::BufferKind;
use super::raster::{draw_vertex_stream, draw_vertex_stream_host, primitive_vertex_count};
use super::{D3D_OK, D3DERR_INVALIDCALL, D3DFMT_INDEX32};
use crate::WinApiState;
use crate::d3d9_render::parse_fvf;

/// Shared body of the two `Draw*UP` handlers.
///
/// `vertex_count` is the vertex pool size (derived from the primitive for the
/// non-indexed form, `NumVertices` for the indexed form); `index_count` is 0
/// for the non-indexed form.
// Wide signature: shared body for the two Draw*UP handlers carrying the full draw command.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_draw_up_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    primitive_type: u64,
    primitive_count: u64,
    data_va: u64,
    stride_raw: u64,
    index_va: u64,
    index_format: u32,
    vertex_count: usize,
    index_count: usize,
) -> Result<u64> {
    let return_value =
        if state.d3d9().d3d9_scene_active == crate::state::SceneState::Active && data_va != 0 {
            match parse_fvf(state.d3d9().d3d9_current_fvf) {
                Some(layout) => {
                    let stride = usize::try_from(stride_raw & u64::from(u32::MAX))
                        .context("DrawPrimitiveUP stride does not fit usize")?;
                    let layout_stride = usize::try_from(layout.stride).unwrap_or(usize::MAX);
                    if stride < layout_stride || (index_count > 0 && index_va == 0) {
                        D3DERR_INVALIDCALL
                    } else {
                        draw_vertex_stream(
                            engine,
                            state,
                            data_va,
                            &layout,
                            stride,
                            vertex_count,
                            primitive_type,
                            primitive_count,
                            index_va,
                            index_format,
                            index_count,
                        )?;
                        D3D_OK
                    }
                }
                None => D3DERR_INVALIDCALL,
            }
        } else {
            D3DERR_INVALIDCALL
        };
    Ok(return_value)
}

/// Shared body of the two buffer-form draws.
///
/// `indexed` is `Some((base_vertex_index, start_index))` for the indexed
/// form; `start_vertex` (the non-indexed form's vertex offset) is folded into
/// the stream base. Reads the `SetStreamSource` vertex buffer and (for the
/// indexed form) the `SetIndices` index buffer from their host records.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_buffer_form_common(
    state: &mut WinApiState,
    primitive_type: u64,
    primitive_count: u64,
    indexed: Option<(u64, u64)>,
    start_vertex: u64,
) -> Result<u64> {
    if state.d3d9().d3d9_scene_active != crate::state::SceneState::Active {
        return Ok(D3DERR_INVALIDCALL);
    }
    let Some(layout) = parse_fvf(state.d3d9().d3d9_current_fvf) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    let (stream_va, stride_u32, stream_offset) = {
        let d3d = state.d3d9();
        (
            d3d.d3d9_stream_source_va,
            d3d.d3d9_stream_stride,
            d3d.d3d9_stream_offset,
        )
    };
    let stride = usize::try_from(stride_u32).unwrap_or(0);
    let layout_stride = usize::try_from(layout.stride).unwrap_or(usize::MAX);
    if stream_va == 0 || stride < layout_stride {
        // No stream, or a stride smaller than the FVF's natural size — real
        // D3D9 rejects both.
        return Ok(D3DERR_INVALIDCALL);
    }

    // Clone the vertex slice out of the record so the mutable `state` borrow
    // below (the rasterizer) is uncontended (the handle_present pattern).
    let stream_base =
        u64::from(stream_offset).saturating_add(start_vertex.saturating_mul(u64::from(stride_u32)));
    let data = {
        let d3d = state.d3d9();
        let Some(record) = d3d.d3d9_buffers.get(&stream_va) else {
            return Ok(D3DERR_INVALIDCALL);
        };
        if !matches!(record.kind, BufferKind::Vertex { .. }) {
            return Ok(D3DERR_INVALIDCALL);
        }
        // The stream slice is everything after the base; the rasterizer skips
        // vertices past the buffer end (out-of-range → no pixels, like the UP
        // path's bad-pointer skip).
        record
            .data
            .get(usize::try_from(stream_base).unwrap_or(usize::MAX)..)
            .unwrap_or(&[])
            .to_vec()
    };
    let vertex_count = primitive_vertex_count(primitive_type, primitive_count).unwrap_or(0);

    let (index_data, index_size, index_offset, vertex_base) = match indexed {
        Some((base_vertex_index, start_index)) => {
            let index_va = state.d3d9().d3d9_index_buffer_va;
            if index_va == 0 {
                return Ok(D3DERR_INVALIDCALL);
            }
            let index_bytes = {
                let d3d = state.d3d9();
                let Some(index_record) = d3d.d3d9_buffers.get(&index_va) else {
                    return Ok(D3DERR_INVALIDCALL);
                };
                let BufferKind::Index { format } = index_record.kind else {
                    return Ok(D3DERR_INVALIDCALL);
                };
                let index_size =
                    usize::try_from(if format == D3DFMT_INDEX32 { 4 } else { 2 }).unwrap_or(2);
                (index_record.data.clone(), index_size)
            };
            // BaseVertexIndex is a signed INT: bit 31 is the sign.
            let base_vertex = i64::try_from(base_vertex_index & u64::from(u32::MAX)).unwrap_or(0);
            let base_vertex = if base_vertex_index & (1_u64 << 31) != 0 {
                base_vertex.wrapping_sub(1_i64 << 32)
            } else {
                base_vertex
            };
            let start = usize::try_from(start_index & u64::from(u32::MAX)).unwrap_or(0);
            (Some(index_bytes.0), index_bytes.1, start, base_vertex)
        }
        None => (None, 0, 0, 0),
    };

    draw_vertex_stream_host(
        state,
        &data,
        &layout,
        stride,
        vertex_count,
        primitive_type,
        primitive_count,
        index_data.as_deref().map(|bytes| (bytes, index_size)),
        index_offset,
        vertex_base,
    )?;
    Ok(D3D_OK)
}
