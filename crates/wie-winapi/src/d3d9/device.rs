//! `IDirect3DDevice9` dispatch handlers: render state (FVF, render state,
//! texture-stage, sampler), device and `IDirect3D9` releases, Present, Clear,
//! scene markers, transforms, viewport, the Draw forms, and the stream /
//! index / vertex-buffer / index-buffer handlers.
//!
//! The `IDirect3D9` surface / adapter / `CreateDevice` API lives in the
//! parent module; shader objects live in `shader`, software rasterization in
//! `raster`, texture/surface lifecycle in `texture`, and blend/depth state in
//! `blend`.

use anyhow::{Context, Result};

use super::raster::{
    draw_vertex_stream, fill_backbuffer_rect, parse_mat4, primitive_vertex_count, read_f32_at,
    read_u32_at,
};
use super::{
    D3D_OK, D3DCLEAR_TARGET, D3DCLEAR_ZBUFFER, D3DERR_INVALIDCALL, D3DTS_PROJECTION, D3DTS_VIEW,
    D3DTS_WORLD, IDIRECT3D9_OBJECT_OFFSET, IDIRECT3DDEVICE9_OBJECT_OFFSET, read_stack_argument,
};
use crate::d3d9_render::{
    D3DRS_ALPHABLENDENABLE, D3DRS_BLENDOP, D3DRS_DESTBLEND, D3DRS_SRCBLEND, D3DRS_ZENABLE,
    D3DRS_ZFUNC, D3DRS_ZWRITEENABLE, D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER,
    D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER, D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP,
    D3DTSS_COLORARG1, D3DTSS_COLORARG2, D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX, D3dBlend, D3dBlendOp,
    D3dCmpFunc, D3dZBufferType, RenderState, TextureStageState, parse_fvf,
};
use crate::guest_memory::write_u64 as write_guest_u64;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// Handles `IDirect3DDevice9::SetFVF`.
pub fn handle_set_fvf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::SetFVF")?;

    let fvf_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::SetFVF")?;

    let fvf_low = fvf_raw & u64::from(u32::MAX);

    let fvf = u32::try_from(fvf_low).context("IDirect3DDevice9::SetFVF value does not fit u32")?;

    state.d3d9().d3d9_current_fvf = fvf;

    let return_value = D3D_OK;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::SetFVF")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetRenderState`.
pub fn handle_set_render_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::SetRenderState")?;

    let render_state_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::SetRenderState")?;

    let value_raw = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DDevice9::SetRenderState")?;

    let render_state = u32::try_from(render_state_raw & u64::from(u32::MAX))
        .context("SetRenderState state identifier does not fit u32")?;

    let value = u32::try_from(value_raw & u64::from(u32::MAX))
        .context("SetRenderState value does not fit u32")?;

    // Decode once at the register boundary into the typed render state. The
    // D3DRS_* values are guest input; unmodeled states have no effect on the
    // software pipeline and are dropped (their GetRenderState reads fall back
    // to 0, D3D9's default for unused states).
    let rs = &mut state.d3d9().d3d9_render_state;
    match render_state {
        D3DRS_ALPHABLENDENABLE => rs.alpha_blend_enable = value != 0,
        D3DRS_ZWRITEENABLE => rs.z_write_enable = value != 0,
        D3DRS_ZENABLE => rs.z_enable = D3dZBufferType::from_u32(value),
        D3DRS_ZFUNC => rs.z_func = D3dCmpFunc::from_u32(value),
        D3DRS_SRCBLEND => rs.src_blend = D3dBlend::from_u32(value),
        D3DRS_DESTBLEND => rs.dest_blend = D3dBlend::from_u32(value),
        D3DRS_BLENDOP => rs.blend_op = D3dBlendOp::from_u32(value),
        _ => {}
    }

    let return_value = D3D_OK;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::SetRenderState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Apply one decoded `D3DTSS_*` slot to a stage's typed state. Unmodeled
/// slots are preserved verbatim so `GetTextureStageState` round-trips them.
fn apply_tss(stage: &mut TextureStageState, slot: u32, value: u32) {
    match slot {
        D3DTSS_COLOROP => stage.color_op = value,
        D3DTSS_COLORARG1 => stage.color_arg1 = value,
        D3DTSS_COLORARG2 => stage.color_arg2 = value,
        D3DTSS_ALPHAOP => stage.alpha_op = value,
        D3DTSS_ALPHAARG1 => stage.alpha_arg1 = value,
        D3DTSS_ALPHAARG2 => stage.alpha_arg2 = value,
        D3DTSS_TEXCOORDINDEX => stage.tex_coord_index = value,
        _ => stage.other_tss.push((slot, value)),
    }
}

/// Apply one decoded `D3DSAMP_*` slot to a stage's typed state. Unmodeled
/// slots are preserved verbatim so `GetSamplerState` round-trips them.
fn apply_sampler(stage: &mut TextureStageState, slot: u32, value: u32) {
    match slot {
        D3DSAMP_ADDRESSU => stage.address_u = value,
        D3DSAMP_ADDRESSV => stage.address_v = value,
        D3DSAMP_MAGFILTER => stage.mag_filter = value,
        D3DSAMP_MINFILTER => stage.min_filter = value,
        D3DSAMP_MIPFILTER => stage.mip_filter = value,
        _ => stage.other_sampler.push((slot, value)),
    }
}

/// Handles `IDirect3DDevice9::SetTextureStageState`.
pub fn handle_set_texture_stage_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::SetTextureStageState")?;

    let stage_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::SetTextureStageState")?;

    let state_type_raw = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DDevice9::SetTextureStageState")?;

    let value_raw = engine
        .read_r9()
        .context("failed to read R9 for IDirect3DDevice9::SetTextureStageState")?;

    let stage = u32::try_from(stage_raw & u64::from(u32::MAX))
        .context("SetTextureStageState stage does not fit u32")?;

    let state_type = u32::try_from(state_type_raw & u64::from(u32::MAX))
        .context("SetTextureStageState state type does not fit u32")?;

    let value = u32::try_from(value_raw & u64::from(u32::MAX))
        .context("SetTextureStageState value does not fit u32")?;

    // Decode once at the register boundary into the typed per-stage state
    // (stages beyond 7 are out of the 8-stage contract and ignored, matching
    // the binding array).
    if let Some(stage_state) = state
        .d3d9()
        .d3d9_stage_states
        .get_mut(usize::try_from(stage).unwrap_or(usize::MAX))
    {
        apply_tss(stage_state, state_type, value);
    }

    let return_value = D3D_OK;

    let return_address = engine.return_from_win64_api(return_value).context(
        "failed to return from \
             IDirect3DDevice9::SetTextureStageState",
    )?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetSamplerState`.
pub fn handle_set_sampler_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this = engine
        .read_rcx()
        .context("failed to read RCX for SetSamplerState")?;

    let sampler = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("sampler does not fit u32")?;

    let state_type = u32::try_from(engine.read_r8()? & u64::from(u32::MAX))
        .context("state type does not fit u32")?;

    let value =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("value does not fit u32")?;

    if let Some(stage_state) = state
        .d3d9()
        .d3d9_stage_states
        .get_mut(usize::try_from(sampler).unwrap_or(usize::MAX))
    {
        apply_sampler(stage_state, state_type, value);
    }

    let return_value = D3D_OK;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetSamplerState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::Release`.
pub fn handle_device_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::Release")?;

    let valid_object = this_pointer != 0 && this_pointer == state.d3d9().d3d9_device_object_address;

    let return_value = if valid_object {
        state.d3d9().d3d9_device_ref_count = state.d3d9().d3d9_device_ref_count.saturating_sub(1);

        let remaining_references = state.d3d9().d3d9_device_ref_count;

        if remaining_references == 0 {
            let allocation_address = this_pointer
                .checked_sub(IDIRECT3DDEVICE9_OBJECT_OFFSET)
                .context("IDirect3DDevice9 allocation address underflow")?;

            let _ = state
                .heap_state
                .heap
                .free_coherent(engine, allocation_address);

            state.d3d9().d3d9_device_object_address = 0;
            state.d3d9().d3d9_current_vertex_shader = 0;
            state.d3d9().d3d9_pixel_shader = 0;
            state.d3d9().d3d9_shaders.clear();
            state.d3d9().d3d9_ps_constants = [[0.0; 4]; crate::d3d9_shader::PS_CONST_COUNT];
            state.d3d9().d3d9_vs_constants = [[0.0; 4]; crate::d3d9_shader::VS_CONST_COUNT];
            state.d3d9().d3d9_current_fvf = 0;
            state.d3d9().d3d9_render_state = RenderState::default();
            state.d3d9().d3d9_stage_states = std::array::from_fn(|_| TextureStageState::default());
            state.d3d9().d3d9_backbuffer.clear();
            state.d3d9().d3d9_backbuffer_width = 0;
            state.d3d9().d3d9_backbuffer_height = 0;
            state.d3d9().d3d9_present_hwnd = crate::handles::Hwnd::NULL;
            state.d3d9().d3d9_scene_active = false;
            state.d3d9().d3d9_dirty = None;
        }

        u64::from(remaining_references)
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::Release")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3D9::Release`.
pub fn handle_direct3d9_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::Release")?;

    let valid_object = this_pointer != 0 && this_pointer == state.d3d9().d3d9_object_address;

    let return_value = if valid_object {
        state.d3d9().d3d9_ref_count = state.d3d9().d3d9_ref_count.saturating_sub(1);

        let remaining_references = state.d3d9().d3d9_ref_count;

        if remaining_references == 0 {
            let allocation_address = this_pointer
                .checked_sub(IDIRECT3D9_OBJECT_OFFSET)
                .context("IDirect3D9 allocation address underflow")?;

            let _ = state
                .heap_state
                .heap
                .free_coherent(engine, allocation_address);

            state.d3d9().d3d9_object_address = 0;
        }

        u64::from(remaining_references)
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::Release")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `IDirect3DDevice9::Present` (vtable slot 17).
///
/// Publishes the backbuffer through `PresentState` as a `SurfaceFrame` — the
/// same pipeline GDI BitBlt uses — scaled (nearest) into the device window's
/// surface when the sizes differ. Slice 1 (B7): Present returns immediately;
/// vsync frame pacing is deferred to P4 and the gating requirement is
/// trivially satisfied by never blocking.
pub fn handle_present(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::Present")?;
    // pSourceRect / pDestRect / hDestWindowOverride / pDirtyRegion are unused
    // in slice 1: the whole backbuffer presents into the device window.

    let (bb_w, bb_h) = (
        state.d3d9().d3d9_backbuffer_width,
        state.d3d9().d3d9_backbuffer_height,
    );
    let hwnd = state.d3d9().d3d9_present_hwnd;
    if bb_w > 0
        && bb_h > 0
        && hwnd != crate::handles::Hwnd::NULL
        && !state.d3d9().d3d9_backbuffer.is_empty()
    {
        let (win_w, win_h) = crate::user32::window_client_size(state, hwnd.as_u64());
        let win_w = u32::try_from(win_w).unwrap_or(1).max(1);
        let win_h = u32::try_from(win_h).unwrap_or(1).max(1);
        state.present().ensure_surface(hwnd, win_w, win_h);
        // Clone the backbuffer so the `d3d9()` borrow ends before `present()`.
        let backbuffer = state.d3d9().d3d9_backbuffer.clone();
        if let Some(surface) = state.present().surfaces.get_mut(&hwnd) {
            if bb_w == win_w && bb_h == win_h {
                let n = surface.pixels.len().min(backbuffer.len());
                if let (Some(dst), Some(src)) = (surface.pixels.get_mut(..n), backbuffer.get(..n)) {
                    dst.copy_from_slice(src);
                }
            } else {
                wie_cpu::stretch_nearest(
                    &mut surface.pixels,
                    &backbuffer,
                    bb_w,
                    bb_h,
                    win_w,
                    win_h,
                );
            }
        }
        state.present().publish(hwnd);
    }
    tracing::trace!(target: "wiegui", bb_w, bb_h, hwnd = hwnd.as_u64(), "D3D9 Present");

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from IDirect3DDevice9::Present")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::Clear` (vtable slot 43).
///
/// `D3DCLEAR_TARGET` fills the backbuffer (the full frame when `pRects` is
/// NULL/Count 0, otherwise each listed `D3DRECT`) with the `D3DCOLOR`
/// (alpha masked). `D3DCLEAR_ZBUFFER` is accepted but clears nothing — slice
/// 1 has no depth surface.
pub fn handle_clear(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::Clear")?;
    let count_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::Clear")?;
    let rects_ptr = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DDevice9::Clear")?;
    let flags_raw = engine
        .read_r9()
        .context("failed to read R9 for IDirect3DDevice9::Clear")?;
    let color_raw = read_stack_argument(engine, 0x28, "IDirect3DDevice9::Clear Color")?;
    let z_raw = read_stack_argument(engine, 0x30, "IDirect3DDevice9::Clear Z")?;
    let _stencil = read_stack_argument(engine, 0x38, "IDirect3DDevice9::Clear Stencil")?;

    let flags =
        u32::try_from(flags_raw & u64::from(u32::MAX)).context("Clear flags do not fit u32")?;
    let clear_target = flags & D3DCLEAR_TARGET != 0;
    let clear_depth = flags & D3DCLEAR_ZBUFFER != 0;
    let z_value = f32::from_bits(u32::try_from(z_raw & u64::from(u32::MAX)).unwrap_or(0));
    if clear_depth {
        // P4c: clear the bound depth buffer (0.0 = near). No depth surface
        // bound → a no-op (documented), matching D3D9's behavior.
        let depth_stencil = state.d3d9().d3d9_depth_stencil;
        if let Some(record) = state.d3d9().d3d9_depth_surfaces.get_mut(&depth_stencil) {
            for slot in &mut record.depth {
                *slot = z_value;
            }
        } else {
            tracing::trace!(target: "wiegui", "D3D9 Clear: ZBUFFER flag ignored (no depth surface)");
        }
    }
    let rect_count = usize::try_from(count_raw & u64::from(u32::MAX))
        .context("Clear rect count does not fit usize")?;
    let color =
        u32::try_from(color_raw & u64::from(u32::MAX)).context("Clear color does not fit u32")?;
    let color_0rgb = color & 0x00FF_FFFF;

    if clear_target {
        let (width, height) = (
            state.d3d9().d3d9_backbuffer_width,
            state.d3d9().d3d9_backbuffer_height,
        );
        if width > 0 && height > 0 {
            let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
            if state.d3d9().d3d9_backbuffer.len() != needed {
                state.d3d9().d3d9_backbuffer = vec![color_0rgb; needed];
            }
            if rects_ptr != 0 && rect_count > 0 {
                let mut rect_bytes = vec![0_u8; rect_count.saturating_mul(16)];
                if engine.mem_read(rects_ptr, &mut rect_bytes).is_ok() {
                    for i in 0..rect_count {
                        let off = i.saturating_mul(16);
                        let left = i32::from_le_bytes(
                            rect_bytes
                                .get(off..off.saturating_add(4))
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let top = i32::from_le_bytes(
                            rect_bytes
                                .get(off.saturating_add(4)..off.saturating_add(8))
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let right = i32::from_le_bytes(
                            rect_bytes
                                .get(off.saturating_add(8)..off.saturating_add(12))
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let bottom = i32::from_le_bytes(
                            rect_bytes
                                .get(off.saturating_add(12)..off.saturating_add(16))
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        fill_backbuffer_rect(
                            &mut state.d3d9().d3d9_backbuffer,
                            width,
                            height,
                            left,
                            top,
                            right,
                            bottom,
                            color_0rgb,
                        );
                    }
                }
            } else {
                for pixel in &mut state.d3d9().d3d9_backbuffer {
                    *pixel = color_0rgb;
                }
            }
            // The whole frame changed — a partial Present region is invalid.
            state.d3d9().d3d9_dirty = None;
        }
    }
    // D3DCLEAR_ZBUFFER: no depth surface in slice 1 — accepted, clears nothing.

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from IDirect3DDevice9::Clear")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}
/// Handles `IDirect3DDevice9::BeginScene` (vtable slot 41).
pub fn handle_begin_scene(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::BeginScene")?;

    let return_value = if state.d3d9().d3d9_scene_active {
        D3DERR_INVALIDCALL
    } else {
        state.d3d9().d3d9_scene_active = true;
        D3D_OK
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::BeginScene")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::EndScene` (vtable slot 42).
///
/// The software rasterizer draws immediately to the backbuffer, so EndScene
/// is a scene-state marker that flushes nothing.
pub fn handle_end_scene(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::EndScene")?;

    let return_value = if state.d3d9().d3d9_scene_active {
        state.d3d9().d3d9_scene_active = false;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::EndScene")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetTransform` (vtable slot 44).
///
/// Stores the world / view / projection matrices (all other transform types
/// are accepted and ignored — texture-space matrices are out of slice 1).
pub fn handle_set_transform(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::SetTransform")?;
    let state_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::SetTransform")?;
    let matrix_ptr = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DDevice9::SetTransform")?;

    let transform_state = u32::try_from(state_raw & u64::from(u32::MAX))
        .context("SetTransform state does not fit u32")?;

    if matrix_ptr != 0 {
        let mut bytes = [0_u8; 64];
        if engine.mem_read(matrix_ptr, &mut bytes).is_ok() {
            let matrix = parse_mat4(&bytes);
            match transform_state {
                D3DTS_WORLD => state.d3d9().d3d9_world_matrix = matrix,
                D3DTS_VIEW => state.d3d9().d3d9_view_matrix = matrix,
                D3DTS_PROJECTION => state.d3d9().d3d9_projection_matrix = matrix,
                _ => {}
            }
        }
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from IDirect3DDevice9::SetTransform")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::SetViewport` (vtable slot 47).
pub fn handle_set_viewport(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::SetViewport")?;
    let viewport_ptr = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::SetViewport")?;

    if viewport_ptr != 0 {
        let mut bytes = [0_u8; 24];
        if engine.mem_read(viewport_ptr, &mut bytes).is_ok() {
            state.d3d9().d3d9_viewport = (
                read_u32_at(&bytes, 0),
                read_u32_at(&bytes, 4),
                read_u32_at(&bytes, 8),
                read_u32_at(&bytes, 12),
                read_f32_at(&bytes, 16),
                read_f32_at(&bytes, 20),
            );
        }
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from IDirect3DDevice9::SetViewport")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetViewport` (vtable slot 48).
pub fn handle_get_viewport(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DDevice9::GetViewport")?;
    let viewport_ptr = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DDevice9::GetViewport")?;

    let return_value = if viewport_ptr != 0 {
        let (x, y, width, height, min_z, max_z) = state.d3d9().d3d9_viewport;
        let mut bytes = [0_u8; 24];
        bytes[0..4].copy_from_slice(&x.to_le_bytes());
        bytes[4..8].copy_from_slice(&y.to_le_bytes());
        bytes[8..12].copy_from_slice(&width.to_le_bytes());
        bytes[12..16].copy_from_slice(&height.to_le_bytes());
        bytes[16..20].copy_from_slice(&min_z.to_le_bytes());
        bytes[20..24].copy_from_slice(&max_z.to_le_bytes());
        engine
            .mem_write(viewport_ptr, &bytes)
            .context("failed to write D3DVIEWPORT9")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DDevice9::GetViewport")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Shared body of the two `Draw*UP` handlers.
///
/// `vertex_count` is the vertex pool size (derived from the primitive for the
/// non-indexed form, `NumVertices` for the indexed form); `index_count` is 0
/// for the non-indexed form.
#[allow(clippy::too_many_arguments)]
fn handle_draw_up_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    primitive_type: u64,
    primitive_count: u64,
    data_ptr: u64,
    stride_raw: u64,
    index_ptr: u64,
    index_format: u32,
    vertex_count: usize,
    index_count: usize,
) -> Result<u64> {
    let return_value = if state.d3d9().d3d9_scene_active && data_ptr != 0 {
        match parse_fvf(state.d3d9().d3d9_current_fvf) {
            Some(layout) => {
                let stride = usize::try_from(stride_raw & u64::from(u32::MAX))
                    .context("DrawPrimitiveUP stride does not fit usize")?;
                let layout_stride = usize::try_from(layout.stride).unwrap_or(usize::MAX);
                if stride < layout_stride || (index_count > 0 && index_ptr == 0) {
                    D3DERR_INVALIDCALL
                } else {
                    draw_vertex_stream(
                        engine,
                        state,
                        data_ptr,
                        &layout,
                        stride,
                        vertex_count,
                        primitive_type,
                        primitive_count,
                        index_ptr,
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

/// Handles `IDirect3DDevice9::DrawPrimitiveUP` (vtable slot 83).
pub fn handle_draw_primitive_up(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let primitive_type = engine
        .read_rdx()
        .context("failed to read RDX for DrawPrimitiveUP")?;
    let primitive_count = engine
        .read_r8()
        .context("failed to read R8 for DrawPrimitiveUP")?;
    let data_ptr = engine
        .read_r9()
        .context("failed to read R9 for DrawPrimitiveUP")?;
    let stride_raw = read_stack_argument(engine, 0x28, "DrawPrimitiveUP VertexStreamZeroStride")?;

    let vertex_count = primitive_vertex_count(primitive_type, primitive_count).unwrap_or(0);
    let return_value = handle_draw_up_common(
        engine,
        &mut *ctx.state,
        primitive_type,
        primitive_count,
        data_ptr,
        stride_raw,
        0,
        0,
        vertex_count,
        0,
    )?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from DrawPrimitiveUP")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::DrawIndexedPrimitiveUP` (vtable slot 84).
pub fn handle_draw_indexed_primitive_up(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let primitive_type = engine
        .read_rdx()
        .context("failed to read RDX for DrawIndexedPrimitiveUP")?;
    let _min_vertex_index = engine
        .read_r8()
        .context("failed to read R8 for DrawIndexedPrimitiveUP")?;
    let num_vertices_raw = engine
        .read_r9()
        .context("failed to read R9 for DrawIndexedPrimitiveUP")?;
    let primitive_count =
        read_stack_argument(engine, 0x28, "DrawIndexedPrimitiveUP PrimitiveCount")?;
    let index_ptr = read_stack_argument(engine, 0x30, "DrawIndexedPrimitiveUP pIndexData")?;
    let index_format_raw =
        read_stack_argument(engine, 0x38, "DrawIndexedPrimitiveUP IndexDataFormat")?;
    let data_ptr =
        read_stack_argument(engine, 0x40, "DrawIndexedPrimitiveUP pVertexStreamZeroData")?;
    let stride_raw = read_stack_argument(
        engine,
        0x48,
        "DrawIndexedPrimitiveUP VertexStreamZeroStride",
    )?;

    let num_vertices = usize::try_from(num_vertices_raw & u64::from(u32::MAX))
        .context("DrawIndexedPrimitiveUP vertex count does not fit usize")?;
    let index_format = u32::try_from(index_format_raw & u64::from(u32::MAX))
        .context("DrawIndexedPrimitiveUP index format does not fit u32")?;
    let index_count = primitive_vertex_count(primitive_type, primitive_count).unwrap_or(0);
    let return_value = handle_draw_up_common(
        engine,
        &mut *ctx.state,
        primitive_type,
        primitive_count,
        data_ptr,
        stride_raw,
        index_ptr,
        index_format,
        num_vertices,
        index_count,
    )?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from DrawIndexedPrimitiveUP")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::DrawPrimitive` (vtable slot 81).
///
/// Slice 1 implements the UP forms only; buffer-form draws require
/// `CreateVertexBuffer`/`Lock` (new COM interfaces), which are deferred. This
/// returns success without drawing — the honest outcome, since no valid
/// vertex buffer can exist while `CreateVertexBuffer` reports failure.
pub fn handle_draw_primitive(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _primitive_type = engine
        .read_rdx()
        .context("failed to read RDX for DrawPrimitive")?;
    let _start_vertex = engine
        .read_r8()
        .context("failed to read R8 for DrawPrimitive")?;
    let _primitive_count = engine
        .read_r9()
        .context("failed to read R9 for DrawPrimitive")?;

    tracing::debug!("DrawPrimitive (buffer form) is a no-op in slice 1");

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from DrawPrimitive")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::DrawIndexedPrimitive` (vtable slot 82).
///
/// Buffer form — a no-op in slice 1 (see [`handle_draw_primitive`]).
pub fn handle_draw_indexed_primitive(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _primitive_type = engine
        .read_rdx()
        .context("failed to read RDX for DrawIndexedPrimitive")?;
    let _base_vertex_index = engine
        .read_r8()
        .context("failed to read R8 for DrawIndexedPrimitive")?;
    let _min_vertex_index = engine
        .read_r9()
        .context("failed to read R9 for DrawIndexedPrimitive")?;
    let _num_vertices = read_stack_argument(engine, 0x28, "DrawIndexedPrimitive NumVertices")?;
    let _start_index = read_stack_argument(engine, 0x30, "DrawIndexedPrimitive StartIndex")?;
    let _primitive_count =
        read_stack_argument(engine, 0x38, "DrawIndexedPrimitive PrimitiveCount")?;

    tracing::debug!("DrawIndexedPrimitive (buffer form) is a no-op in slice 1");

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from DrawIndexedPrimitive")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::SetStreamSource` (vtable slot 100).
///
/// Stores stream 0 (used by the deferred buffer-form draws); returns success.
pub fn handle_set_stream_source(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetStreamSource")?;
    let stream_number = engine
        .read_rdx()
        .context("failed to read RDX for SetStreamSource")?;
    let stream_data = engine
        .read_r8()
        .context("failed to read R8 for SetStreamSource")?;
    let _offset_in_bytes = engine
        .read_r9()
        .context("failed to read R9 for SetStreamSource")?;
    let stride_raw = read_stack_argument(engine, 0x28, "SetStreamSource Stride")?;

    if stream_number == 0 {
        state.d3d9().d3d9_stream_source_va = stream_data;
        state.d3d9().d3d9_stream_stride =
            u32::try_from(stride_raw & u64::from(u32::MAX)).unwrap_or(0);
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetStreamSource")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::SetIndices` (vtable slot 104).
pub fn handle_set_indices(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetIndices")?;
    let index_data = engine
        .read_rdx()
        .context("failed to read RDX for SetIndices")?;
    state.d3d9().d3d9_index_buffer_va = index_data;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetIndices")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::CreateVertexBuffer` (vtable slot 26).
///
/// Slice 1 does not implement buffer-form rendering (no vertex-buffer COM
/// objects / Lock), so this honestly reports failure — games using buffers
/// fall back or bail instead of silently rendering nothing.
pub fn handle_create_vertex_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _length = engine
        .read_rdx()
        .context("failed to read RDX for CreateVertexBuffer")?;
    let _usage = engine
        .read_r8()
        .context("failed to read R8 for CreateVertexBuffer")?;
    let _fvf = engine
        .read_r9()
        .context("failed to read R9 for CreateVertexBuffer")?;
    let pp_buffer = read_stack_argument(engine, 0x30, "CreateVertexBuffer ppBuffer")?;
    if pp_buffer != 0 {
        write_guest_u64(engine, pp_buffer, 0)
            .context("failed to clear CreateVertexBuffer output pointer")?;
    }

    tracing::debug!("CreateVertexBuffer unsupported in slice 1 (UP forms only)");

    let return_address = engine
        .return_from_win64_api(D3DERR_INVALIDCALL)
        .context("failed to return from CreateVertexBuffer")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3DERR_INVALIDCALL,
    })
}

/// Handles `IDirect3DDevice9::CreateIndexBuffer` (vtable slot 27).
///
/// Slice 1 does not implement buffer-form rendering (see
/// [`handle_create_vertex_buffer`]).
pub fn handle_create_index_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _length = engine
        .read_rdx()
        .context("failed to read RDX for CreateIndexBuffer")?;
    let _usage = engine
        .read_r8()
        .context("failed to read R8 for CreateIndexBuffer")?;
    let _format = engine
        .read_r9()
        .context("failed to read R9 for CreateIndexBuffer")?;
    let pp_buffer = read_stack_argument(engine, 0x30, "CreateIndexBuffer ppBuffer")?;
    if pp_buffer != 0 {
        write_guest_u64(engine, pp_buffer, 0)
            .context("failed to clear CreateIndexBuffer output pointer")?;
    }

    tracing::debug!("CreateIndexBuffer unsupported in slice 1 (UP forms only)");

    let return_address = engine
        .return_from_win64_api(D3DERR_INVALIDCALL)
        .context("failed to return from CreateIndexBuffer")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3DERR_INVALIDCALL,
    })
}
