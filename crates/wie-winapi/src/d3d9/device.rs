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

use super::buffer::{BufferKind, create_buffer_record};
use super::draw::{draw_buffer_form_common, handle_draw_up_common};
use super::raster::{
    fill_backbuffer_rect, parse_mat4, primitive_vertex_count, read_f32_at, read_u32_at,
};
use super::{
    D3D_OK, D3DCLEAR_TARGET, D3DCLEAR_ZBUFFER, D3DERR_INVALIDCALL, D3DFMT_INDEX16, D3DFMT_INDEX32,
    D3DTS_PROJECTION, D3DTS_VIEW, D3DTS_WORLD, IDIRECT3D9_OBJECT_OFFSET,
    IDIRECT3DDEVICE9_OBJECT_OFFSET, MAX_TEXTURE_DIMENSION, read_stack_argument,
};
use crate::d3d9_render::{
    D3DCOLOR_RGB_MASK, D3DRS_ALPHABLENDENABLE, D3DRS_ALPHAFUNC, D3DRS_ALPHAREF,
    D3DRS_ALPHATESTENABLE, D3DRS_BLENDOP, D3DRS_DESTBLEND, D3DRS_FOGCOLOR, D3DRS_FOGDENSITY,
    D3DRS_FOGENABLE, D3DRS_FOGEND, D3DRS_FOGSTART, D3DRS_FOGTABLEMODE, D3DRS_FOGVERTEXMODE,
    D3DRS_SCISSORTESTENABLE, D3DRS_SRCBLEND, D3DRS_ZENABLE, D3DRS_ZFUNC, D3DRS_ZWRITEENABLE,
    D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER, D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER,
    D3DTS_TEXTURE0, D3DTS_TEXTURE7, D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP,
    D3DTSS_COLORARG1, D3DTSS_COLORARG2, D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX, D3dBlend, D3dBlendOp,
    D3dCmpFunc, D3dZBufferType, RenderState, TextureStageState, mat4_mul, parse_fvf,
};
use crate::fake_va::D3d9Iface;
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::{write_u32 as write_guest_u32, write_u64 as write_guest_u64};
use crate::kernel32::low_u32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// Handles `IDirect3DDevice9::SetFVF`.
pub fn handle_set_fvf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::SetFVF")?;

    let fvf_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::SetFVF")?;

    let fvf_low = fvf_raw & u64::from(u32::MAX);

    let fvf = u32::try_from(fvf_low).context("IDirect3DDevice9::SetFVF value does not fit u32")?;

    state.d3d9().d3d9_current_fvf = fvf;

    let return_value = D3D_OK;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetRenderState`.
pub fn handle_set_render_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::SetRenderState")?;

    let render_state_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::SetRenderState")?;

    let value_raw = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::SetRenderState")?;

    let render_state = low_u32(render_state_raw, "SetRenderState state identifier")?;

    let value = low_u32(value_raw, "SetRenderState value")?;

    // L3 validation: an out-of-range value is the honest D3DERR_INVALIDCALL,
    // never a silent accept. Unmodeled states skip the matrix (their raw
    // values round-trip untouched).
    let return_value = if !crate::d3d9_render::render_state_value_valid(render_state, value) {
        D3DERR_INVALIDCALL
    } else {
        // Decode once at the register boundary into the typed render state.
        // The D3DRS_* values are guest input; unmodeled states are preserved
        // verbatim in the raw-value layer so GetRenderState round-trips the
        // last-set value (the L3 fidelity rule — no more 0 for "ignored").
        let d3d = state.d3d9();
        let rs = &mut d3d.d3d9_render_state;
        match render_state {
            D3DRS_ALPHABLENDENABLE => rs.alpha_blend_enable = value != 0,
            D3DRS_ZWRITEENABLE => rs.z_write_enable = value != 0,
            D3DRS_ZENABLE => rs.z_enable = D3dZBufferType::from_u32(value),
            D3DRS_ZFUNC => rs.z_func = D3dCmpFunc::from_u32(value),
            D3DRS_SRCBLEND => rs.src_blend = D3dBlend::from_u32(value),
            D3DRS_DESTBLEND => rs.dest_blend = D3dBlend::from_u32(value),
            D3DRS_BLENDOP => rs.blend_op = D3dBlendOp::from_u32(value),
            // ── L3 fragment stages ──
            D3DRS_FOGENABLE => rs.fog_enable = value != 0,
            D3DRS_FOGCOLOR => rs.fog_color = value,
            D3DRS_FOGSTART => rs.fog_start = f32::from_bits(value),
            D3DRS_FOGEND => rs.fog_end = f32::from_bits(value),
            D3DRS_FOGDENSITY => rs.fog_density = f32::from_bits(value),
            D3DRS_FOGTABLEMODE => rs.fog_table_mode = value,
            D3DRS_FOGVERTEXMODE => rs.fog_vertex_mode = value,
            D3DRS_ALPHATESTENABLE => rs.alpha_test_enable = value != 0,
            D3DRS_ALPHAFUNC => rs.alpha_func = D3dCmpFunc::from_u32(value),
            D3DRS_ALPHAREF => rs.alpha_ref = u8::try_from(value).unwrap_or(0),
            D3DRS_SCISSORTESTENABLE => rs.scissor_test_enable = value != 0,
            // Unmodeled state: preserve the raw value for the round-trip.
            _ => {
                d3d.d3d9_render_state_raw.insert(render_state, value);
            }
        }
        D3D_OK
    };

    ctx.finish(return_value)
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
    let _this_pointer = read_arg(
        engine,
        ArgReg::Rcx,
        "IDirect3DDevice9::SetTextureStageState",
    )?;

    let stage_raw = read_arg(
        engine,
        ArgReg::Rdx,
        "IDirect3DDevice9::SetTextureStageState",
    )?;

    let state_type_raw = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::SetTextureStageState")?;

    let value_raw = read_arg(engine, ArgReg::R9, "IDirect3DDevice9::SetTextureStageState")?;

    let stage = low_u32(stage_raw, "SetTextureStageState stage")?;

    let state_type = low_u32(state_type_raw, "SetTextureStageState state type")?;

    let value = low_u32(value_raw, "SetTextureStageState value")?;

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

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetSamplerState`.
pub fn handle_set_sampler_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this = read_arg(engine, ArgReg::Rcx, "SetSamplerState")?;

    let sampler = low_u32(engine.read_rdx()?, "sampler")?;

    let state_type = low_u32(engine.read_r8()?, "state type")?;

    let value = low_u32(engine.read_r9()?, "value")?;

    if let Some(stage_state) = state
        .d3d9()
        .d3d9_stage_states
        .get_mut(usize::try_from(sampler).unwrap_or(usize::MAX))
    {
        apply_sampler(stage_state, state_type, value);
    }

    let return_value = D3D_OK;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::Release`.
pub fn handle_device_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::Release")?;

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
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, allocation_address);

            state.d3d9().d3d9_device_object_address = 0;
            state.d3d9().d3d9_current_vertex_shader = 0;
            state.d3d9().d3d9_pixel_shader = 0;
            state.d3d9().d3d9_shaders.clear();
            state.d3d9().d3d9_ps_constants = [[0.0; 4]; crate::d3d9_shader::PS_CONST_COUNT];
            state.d3d9().d3d9_vs_constants = [[0.0; 4]; crate::d3d9_shader::VS_CONST_COUNT];
            state.d3d9().d3d9_vs_int_constants = [[0; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT];
            state.d3d9().d3d9_vs_bool_constants = [false; crate::d3d9_shader::VS_BOOL_CONST_COUNT];
            state.d3d9().d3d9_current_fvf = 0;
            state.d3d9().d3d9_render_state = RenderState::default();
            state.d3d9().d3d9_render_state_raw.clear();
            state.d3d9().d3d9_scissor_rect = None;
            state.d3d9().d3d9_texture_matrices = [crate::d3d9_render::IDENTITY; 8];
            state.d3d9().d3d9_stage_states = std::array::from_fn(|_| TextureStageState::default());
            state.d3d9().d3d9_backbuffer.clear();
            state.d3d9().d3d9_backbuffer_width = 0;
            state.d3d9().d3d9_backbuffer_height = 0;
            state.d3d9().d3d9_present_hwnd = crate::handles::Hwnd::NULL;
            state.d3d9().d3d9_scene_active = crate::state::SceneState::Inactive;
            state.d3d9().d3d9_dirty = None;
            // L2: the vertex/index buffers are device-owned COM objects — drop
            // their records (their vtable blocks are freed by the guest's own
            // Release calls, which normally precede device teardown).
            state.d3d9().d3d9_buffers.clear();
            state.d3d9().d3d9_stream_source_va = 0;
            state.d3d9().d3d9_stream_stride = 0;
            state.d3d9().d3d9_stream_offset = 0;
            state.d3d9().d3d9_index_buffer_va = 0;
            // Wave 2: drop the capture bookkeeping with the device — an
            // unflushed stream and the seen/dirty sets reference VAs that
            // teardown just invalidated.
            state.d3d9().d3d9_capture_stream.clear();
            state.d3d9().d3d9_capture_emu_dirty_rt.clear();
            state.d3d9().d3d9_capture_rt_seen.clear();
            state.d3d9().d3d9_capture_depth_seen.clear();
        }

        u64::from(remaining_references)
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3D9::Release`.
pub fn handle_direct3d9_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::Release")?;

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
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, allocation_address);

            state.d3d9().d3d9_object_address = 0;
        }

        u64::from(remaining_references)
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Handles `IDirect3DDevice9::Present` (vtable slot 17).
///
/// Publishes the backbuffer through `PresentState` as a `SurfaceFrame` — the
/// same pipeline GDI BitBlt uses — scaled (nearest) into the device window's
/// surface when the sizes differ. Slice 1 (B7): Present returns immediately;
/// vsync frame pacing is deferred to P4 and the gating requirement is
/// trivially satisfied by never blocking.
///
/// Opt-in pacing: `WIE_PRESENT_PACING_HZ=<n>` sleeps the Present handler so
/// the guest renders at ~n FPS (a diagnostic knob for FPS measurements — a
/// guest at thousands of FPS floods the publish path and the profile stops
/// saying anything about steady-state frame cost). Default is off
/// (unpaced). The sleep runs inside the handler under the WinAPI lock, so it
/// pauses guest dispatch on this thread by construction — do not enable it
/// for throughput runs.
pub fn handle_present(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::Present")?;
    // pSourceRect / pDestRect / hDestWindowOverride / pDirtyRegion are unused
    // in slice 1: the whole backbuffer presents into the device window.

    present_pacing_wait(state);

    let (bb_w, bb_h, hwnd) = {
        let d = state.d3d9();
        (
            d.d3d9_backbuffer_width,
            d.d3d9_backbuffer_height,
            d.d3d9_present_hwnd,
        )
    };
    if state.present().channel.capture_enabled() {
        // Wave 2 slice 2: the capture path — flush the recorded op stream to
        // the capture render thread (which replays it into its own backbuffer
        // and publishes). The emu thread's backbuffer is not involved: the
        // render thread owns the implicit backbuffer outright.
        if bb_w > 0 && bb_h > 0 && hwnd != crate::handles::Hwnd::NULL {
            let (win_w, win_h) = crate::user32::window_client_size(state, hwnd.as_u64());
            let win_w = u32::try_from(win_w).unwrap_or(1).max(1);
            let win_h = u32::try_from(win_h).unwrap_or(1).max(1);
            super::capture::flush_present(state, win_w, win_h);
        }
    } else if bb_w > 0
        && bb_h > 0
        && hwnd != crate::handles::Hwnd::NULL
        && !state.d3d9().d3d9_backbuffer.is_empty()
    {
        let (win_w, win_h) = crate::user32::window_client_size(state, hwnd.as_u64());
        let win_w = u32::try_from(win_w).unwrap_or(1).max(1);
        let win_h = u32::try_from(win_h).unwrap_or(1).max(1);
        if state.present().channel.commit_enabled() {
            // Wave 2 (Option A1): commit mode — hand the finished backbuffer
            // to the render thread (a pointer move, no copy) and draw the
            // next frame into a recycled buffer. The stretch + publish happen
            // off the big lock.
            let backbuffer = std::mem::take(&mut state.d3d9().d3d9_backbuffer);
            let recycled = state
                .present()
                .enqueue_present_commit(hwnd, bb_w, bb_h, win_w, win_h, backbuffer);
            // Keep the "backbuffer.len() == w * h" invariant: a recycled
            // spare is already the right size; a cold pool allocates once.
            state.d3d9().d3d9_backbuffer = if recycled.len()
                == usize::try_from(bb_w.checked_mul(bb_h).unwrap_or(0)).unwrap_or(0)
            {
                recycled
            } else {
                vec![0_u32; usize::try_from(bb_w.checked_mul(bb_h).unwrap_or(0)).unwrap_or(0)]
            };
        } else {
            // Q9/C: in-place pooled target — take the backbuffer without cloning
            // (capacity retained, no per-present alloc) and write directly into the
            // pooled WindowSurface slice that `ensure_surface` will hand back via
            // Q2/D. No intermediate `Vec<u32>` copy; size mismatch keeps
            // `stretch_nearest` but writes directly into the pooled slice.
            let backbuffer = std::mem::take(&mut state.d3d9().d3d9_backbuffer);
            let bb_slice: &[u32] = &backbuffer;
            // Hand the next pooled surface slice to the present as render target
            // (Q9/C). `blit_frame` writes directly into that pooled allocation.
            state.present().ensure_surface(hwnd, win_w, win_h);
            // Direct blit into the pooled slice — no temp Vec, no intermediate copy.
            state.present().blit_frame(hwnd, bb_slice, bb_w, bb_h);
            // Restore the backbuffer Vec with its original capacity for the next draws.
            state.d3d9().d3d9_backbuffer = backbuffer;
        }
    }
    tracing::trace!(target: "wiegui", bb_w, bb_h, hwnd = hwnd.as_u64(), "D3D9 Present");

    ctx.finish(D3D_OK)
}

/// The resolved `WIE_PRESENT_PACING_HZ` target frame interval (`None` = off).
fn present_pacing_period() -> Option<std::time::Duration> {
    static PERIOD: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *PERIOD.get_or_init(|| {
        let hz = std::env::var("WIE_PRESENT_PACING_HZ")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|hz| *hz > 0.0)?;
        Some(std::time::Duration::from_secs_f64(1.0 / hz))
    })
}

/// Sleep out the remainder of the paced frame interval before a Present (see
/// the `WIE_PRESENT_PACING_HZ` doc on [`handle_present`]).
fn present_pacing_wait(state: &mut WinApiState) {
    let Some(period) = present_pacing_period() else {
        return;
    };
    let now = std::time::Instant::now();
    let wait = match state.d3d9().d3d9_last_present {
        Some(last) => {
            let elapsed = now.saturating_duration_since(last);
            period.saturating_sub(elapsed)
        }
        None => std::time::Duration::ZERO,
    };
    state.d3d9().d3d9_last_present = Some(now);
    // Skip sleeps too short to be worth the wake-up cost (sub-millisecond).
    if wait > std::time::Duration::from_millis(1) {
        std::thread::sleep(wait);
    }
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
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::Clear")?;
    let count_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::Clear")?;
    let rects_va = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::Clear")?;
    let flags_raw = read_arg(engine, ArgReg::R9, "IDirect3DDevice9::Clear")?;
    let color_raw = read_stack_argument(engine, 0x28, "IDirect3DDevice9::Clear Color")?;
    let z_raw = read_stack_argument(engine, 0x30, "IDirect3DDevice9::Clear Z")?;
    let _stencil = read_stack_argument(engine, 0x38, "IDirect3DDevice9::Clear Stencil")?;

    let flags = low_u32(flags_raw, "IDirect3DDevice9::Clear flags")?;
    let clear_target = flags & D3DCLEAR_TARGET != 0;
    let clear_depth = flags & D3DCLEAR_ZBUFFER != 0;
    let z_value = f32::from_bits(u32::try_from(z_raw & u64::from(u32::MAX)).unwrap_or(0));
    if clear_depth {
        // Clear the bound depth buffer (0.0 = near). No depth surface
        // bound → a no-op (documented), matching D3D9's behavior.
        let depth_stencil = state.d3d9().d3d9_depth_stencil;
        if let Some(record) = state.d3d9().d3d9_depth_surfaces.get_mut(&depth_stencil) {
            for slot in std::sync::Arc::make_mut(&mut record.depth) {
                *slot = z_value;
            }
        } else {
            tracing::trace!(target: "wiegui", "D3D9 Clear: ZBUFFER flag ignored (no depth surface)");
        }
    }
    let rect_count = usize::try_from(count_raw & u64::from(u32::MAX))
        .context("Clear rect count does not fit usize")?;
    let color = low_u32(color_raw, "Clear color")?;
    let color_0rgb = color & D3DCOLOR_RGB_MASK;

    if super::capture::capture_enabled(state) {
        // Wave 2: record the clear as a self-contained op — the capture
        // render thread performs the fills into its own target copies.
        let mut rects: Vec<crate::gdi32::IRect> = Vec::new();
        if clear_target && rects_va != 0 && rect_count > 0 {
            let mut rect_bytes = vec![0_u8; rect_count.saturating_mul(16)];
            if engine.mem_read(rects_va, &mut rect_bytes).is_ok() {
                for chunk in rect_bytes.chunks(16).take(rect_count) {
                    let i32_at = |offset: usize| {
                        i32::from_le_bytes(
                            chunk
                                .get(offset..offset.saturating_add(4))
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        )
                    };
                    rects.push(crate::gdi32::IRect {
                        left: i32_at(0),
                        top: i32_at(4),
                        right: i32_at(8),
                        bottom: i32_at(12),
                    });
                }
            }
        }
        let (rt, depth_stencil) = {
            let d3d = state.d3d9();
            (d3d.d3d9_render_target, d3d.d3d9_depth_stencil)
        };
        super::capture::record_clear(
            state,
            rt,
            depth_stencil,
            clear_target,
            clear_depth,
            color_0rgb,
            z_value,
            rects,
        );
        return ctx.finish(D3D_OK);
    }

    if clear_target {
        // L6: Clear targets the bound render target when one is set, else
        // the implicit backbuffer. The RT's own dims size the clear.
        let rt = state.d3d9().d3d9_render_target;
        let (width, height) = if rt == 0 {
            (
                state.d3d9().d3d9_backbuffer_width,
                state.d3d9().d3d9_backbuffer_height,
            )
        } else {
            state
                .d3d9()
                .d3d9_render_targets
                .get(&rt)
                .map_or((0, 0), |record| (record.width, record.height))
        };
        if width > 0 && height > 0 {
            let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
            if rt == 0 {
                if state.d3d9().d3d9_backbuffer.len() != needed {
                    state.d3d9().d3d9_backbuffer = vec![color_0rgb; needed];
                }
            } else if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt)
                && record.pixels.len() != needed
            {
                record.pixels = std::sync::Arc::new(vec![color_0rgb; needed]);
            }
            if rects_va != 0 && rect_count > 0 {
                let mut rect_bytes = vec![0_u8; rect_count.saturating_mul(16)];
                if engine.mem_read(rects_va, &mut rect_bytes).is_ok() {
                    for chunk in rect_bytes.chunks(16).take(rect_count) {
                        let left = i32::from_le_bytes(
                            chunk
                                .get(0..4)
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let top = i32::from_le_bytes(
                            chunk
                                .get(4..8)
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let right = i32::from_le_bytes(
                            chunk
                                .get(8..12)
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let bottom = i32::from_le_bytes(
                            chunk
                                .get(12..16)
                                .and_then(|s| s.try_into().ok())
                                .unwrap_or([0; 4]),
                        );
                        let output: &mut [u32] = if rt == 0 {
                            &mut state.d3d9().d3d9_backbuffer
                        } else {
                            state.d3d9().d3d9_render_targets.get_mut(&rt).map_or(
                                &mut [][..],
                                |record| {
                                    std::sync::Arc::make_mut(&mut record.pixels).as_mut_slice()
                                },
                            )
                        };
                        fill_backbuffer_rect(
                            output, width, height, left, top, right, bottom, color_0rgb,
                        );
                    }
                }
            } else if rt == 0 {
                for pixel in &mut state.d3d9().d3d9_backbuffer {
                    *pixel = color_0rgb;
                }
            } else if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt) {
                for pixel in std::sync::Arc::make_mut(&mut record.pixels) {
                    *pixel = color_0rgb;
                }
            }
            // The whole frame changed — a partial Present region is invalid.
            if rt == 0 {
                state.d3d9().d3d9_dirty = None;
            }
        }
    }
    // D3DCLEAR_ZBUFFER: no depth surface in slice 1 — accepted, clears nothing.

    ctx.finish(D3D_OK)
}
/// Handles `IDirect3DDevice9::BeginScene` (vtable slot 41).
pub fn handle_begin_scene(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::BeginScene")?;

    let return_value = if state.d3d9().d3d9_scene_active == crate::state::SceneState::Active {
        D3DERR_INVALIDCALL
    } else {
        state.d3d9().d3d9_scene_active = crate::state::SceneState::Active;
        D3D_OK
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::EndScene` (vtable slot 42).
///
/// The software rasterizer draws immediately to the backbuffer, so EndScene
/// is a scene-state marker that flushes nothing.
pub fn handle_end_scene(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::EndScene")?;

    let return_value = if state.d3d9().d3d9_scene_active == crate::state::SceneState::Active {
        state.d3d9().d3d9_scene_active = crate::state::SceneState::Inactive;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetTransform` (vtable slot 44).
///
/// Stores the world / view / projection matrices and the `D3DTS_TEXTURE0..7`
/// texture-space matrices (the latter stored for `GetTransform` /
/// `MultiplyTransform` round-trips — the actual texgen is a later slice).
pub fn handle_set_transform(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::SetTransform")?;
    let state_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::SetTransform")?;
    let matrix_va = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::SetTransform")?;

    let transform_state = low_u32(state_raw, "SetTransform state")?;

    if matrix_va != 0 {
        let mut bytes = [0_u8; 64];
        if engine.mem_read(matrix_va, &mut bytes).is_ok() {
            let matrix = parse_mat4(&bytes);
            match transform_state {
                D3DTS_WORLD => state.d3d9().d3d9_world_matrix = matrix,
                D3DTS_VIEW => state.d3d9().d3d9_view_matrix = matrix,
                D3DTS_PROJECTION => state.d3d9().d3d9_projection_matrix = matrix,
                D3DTS_TEXTURE0..=D3DTS_TEXTURE7 => {
                    let index =
                        usize::try_from(transform_state - D3DTS_TEXTURE0).unwrap_or(usize::MAX);
                    if let Some(slot) = state.d3d9().d3d9_texture_matrices.get_mut(index) {
                        *slot = matrix;
                    }
                }
                _ => {}
            }
        }
    }

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetTransform` (vtable slot 45).
///
/// Round-trip getter: writes the stored world / view / projection / texture
/// matrix back to the guest. An unknown transform state is the honest
/// `D3DERR_INVALIDCALL` (D3D9 rejects it too).
pub fn handle_get_transform(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::GetTransform")?;
    let state_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::GetTransform")?;
    let matrix_va = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::GetTransform")?;

    let transform_state = low_u32(state_raw, "GetTransform state")?;

    let stored = match transform_state {
        D3DTS_WORLD => Some(state.d3d9().d3d9_world_matrix),
        D3DTS_VIEW => Some(state.d3d9().d3d9_view_matrix),
        D3DTS_PROJECTION => Some(state.d3d9().d3d9_projection_matrix),
        D3DTS_TEXTURE0..=D3DTS_TEXTURE7 => {
            let index = usize::try_from(transform_state - D3DTS_TEXTURE0).unwrap_or(usize::MAX);
            state.d3d9().d3d9_texture_matrices.get(index).copied()
        }
        _ => None,
    };

    let return_value = match stored {
        Some(matrix) if matrix_va != 0 => {
            let mut bytes = [0_u8; 64];
            for (index, value) in matrix.iter().enumerate() {
                let start = index.saturating_mul(4);
                let end = start.saturating_add(4);
                if let Some(slot) = bytes.get_mut(start..end) {
                    slot.copy_from_slice(&value.to_le_bytes());
                }
            }
            engine
                .mem_write(matrix_va, &bytes)
                .context("failed to write D3DMATRIX")?;
            D3D_OK
        }
        _ => D3DERR_INVALIDCALL,
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::MultiplyTransform` (vtable slot 46).
///
/// Real matrix multiply in the D3D9 row-vector convention: the stored
/// transform becomes `current × pMatrix` (the pMatrix is applied after the
/// current one). An unknown transform state is `D3DERR_INVALIDCALL`.
pub fn handle_multiply_transform(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::MultiplyTransform")?;
    let state_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::MultiplyTransform")?;
    let matrix_va = read_arg(engine, ArgReg::R8, "IDirect3DDevice9::MultiplyTransform")?;

    let transform_state = low_u32(state_raw, "MultiplyTransform state")?;

    let return_value = if matrix_va != 0 {
        let mut bytes = [0_u8; 64];
        let readable = engine.mem_read(matrix_va, &mut bytes).is_ok();
        if readable {
            let matrix = parse_mat4(&bytes);
            match transform_state {
                D3DTS_WORLD => {
                    let current = state.d3d9().d3d9_world_matrix;
                    state.d3d9().d3d9_world_matrix = mat4_mul(&current, &matrix);
                    D3D_OK
                }
                D3DTS_VIEW => {
                    let current = state.d3d9().d3d9_view_matrix;
                    state.d3d9().d3d9_view_matrix = mat4_mul(&current, &matrix);
                    D3D_OK
                }
                D3DTS_PROJECTION => {
                    let current = state.d3d9().d3d9_projection_matrix;
                    state.d3d9().d3d9_projection_matrix = mat4_mul(&current, &matrix);
                    D3D_OK
                }
                D3DTS_TEXTURE0..=D3DTS_TEXTURE7 => {
                    let index =
                        usize::try_from(transform_state - D3DTS_TEXTURE0).unwrap_or(usize::MAX);
                    if let Some(current) = state.d3d9().d3d9_texture_matrices.get(index) {
                        let combined = mat4_mul(current, &matrix);
                        if let Some(slot) = state.d3d9().d3d9_texture_matrices.get_mut(index) {
                            *slot = combined;
                        }
                        D3D_OK
                    } else {
                        D3DERR_INVALIDCALL
                    }
                }
                _ => D3DERR_INVALIDCALL,
            }
        } else {
            D3DERR_INVALIDCALL
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetScissorRect` (vtable slot 115).
///
/// Stores the screen-space `RECT` (exclusive right/bottom, like GDI). The
/// fragment stage clips against it when `D3DRS_SCISSORTESTENABLE` is set.
pub fn handle_set_scissor_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::SetScissorRect")?;
    let rect_va = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::SetScissorRect")?;

    let return_value = if rect_va != 0 {
        let mut bytes = [0_u8; 16];
        if engine.mem_read(rect_va, &mut bytes).is_ok() {
            let i32_at = |offset: usize| {
                i32::from_le_bytes(
                    bytes
                        .get(offset..offset.saturating_add(4))
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 4]),
                )
            };
            state.d3d9().d3d9_scissor_rect = Some(crate::gdi32::IRect {
                left: i32_at(0),
                top: i32_at(4),
                right: i32_at(8),
                bottom: i32_at(12),
            });
            D3D_OK
        } else {
            D3DERR_INVALIDCALL
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetViewport` (vtable slot 47).
pub fn handle_set_viewport(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::SetViewport")?;
    let viewport_va = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::SetViewport")?;

    if viewport_va != 0 {
        let mut bytes = [0_u8; 24];
        if engine.mem_read(viewport_va, &mut bytes).is_ok() {
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

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetViewport` (vtable slot 48).
pub fn handle_get_viewport(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::GetViewport")?;
    let viewport_va = read_arg(engine, ArgReg::Rdx, "IDirect3DDevice9::GetViewport")?;

    let return_value = if viewport_va != 0 {
        let (x, y, width, height, min_z, max_z) = state.d3d9().d3d9_viewport;
        let mut bytes = [0_u8; 24];
        bytes[0..4].copy_from_slice(&x.to_le_bytes());
        bytes[4..8].copy_from_slice(&y.to_le_bytes());
        bytes[8..12].copy_from_slice(&width.to_le_bytes());
        bytes[12..16].copy_from_slice(&height.to_le_bytes());
        bytes[16..20].copy_from_slice(&min_z.to_le_bytes());
        bytes[20..24].copy_from_slice(&max_z.to_le_bytes());
        engine
            .mem_write(viewport_va, &bytes)
            .context("failed to write D3DVIEWPORT9")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::DrawPrimitiveUP` (vtable slot 83).
pub fn handle_draw_primitive_up(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let primitive_type = read_arg(engine, ArgReg::Rdx, "DrawPrimitiveUP")?;
    let primitive_count = read_arg(engine, ArgReg::R8, "DrawPrimitiveUP")?;
    let data_va = read_arg(engine, ArgReg::R9, "DrawPrimitiveUP")?;
    let stride_raw = read_stack_argument(engine, 0x28, "DrawPrimitiveUP VertexStreamZeroStride")?;

    let vertex_count = primitive_vertex_count(primitive_type, primitive_count).unwrap_or(0);
    let return_value = handle_draw_up_common(
        engine,
        &mut *ctx.state,
        primitive_type,
        primitive_count,
        data_va,
        stride_raw,
        0,
        0,
        vertex_count,
        0,
    )?;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::DrawIndexedPrimitiveUP` (vtable slot 84).
pub fn handle_draw_indexed_primitive_up(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let primitive_type = read_arg(engine, ArgReg::Rdx, "DrawIndexedPrimitiveUP")?;
    let _min_vertex_index = read_arg(engine, ArgReg::R8, "DrawIndexedPrimitiveUP")?;
    let num_vertices_raw = read_arg(engine, ArgReg::R9, "DrawIndexedPrimitiveUP")?;
    let primitive_count =
        read_stack_argument(engine, 0x28, "DrawIndexedPrimitiveUP PrimitiveCount")?;
    let index_va = read_stack_argument(engine, 0x30, "DrawIndexedPrimitiveUP pIndexData")?;
    let index_format_raw =
        read_stack_argument(engine, 0x38, "DrawIndexedPrimitiveUP IndexDataFormat")?;
    let data_va =
        read_stack_argument(engine, 0x40, "DrawIndexedPrimitiveUP pVertexStreamZeroData")?;
    let stride_raw = read_stack_argument(
        engine,
        0x48,
        "DrawIndexedPrimitiveUP VertexStreamZeroStride",
    )?;

    let num_vertices = usize::try_from(num_vertices_raw & u64::from(u32::MAX))
        .context("DrawIndexedPrimitiveUP vertex count does not fit usize")?;
    let index_format = low_u32(index_format_raw, "DrawIndexedPrimitiveUP index format")?;
    let index_count = primitive_vertex_count(primitive_type, primitive_count).unwrap_or(0);
    let return_value = handle_draw_up_common(
        engine,
        &mut *ctx.state,
        primitive_type,
        primitive_count,
        data_va,
        stride_raw,
        index_va,
        index_format,
        num_vertices,
        index_count,
    )?;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::DrawPrimitive` (vtable slot 81) — the
/// buffer-form non-indexed draw.
///
/// L2: real — reads the vertex buffer bound by `SetStreamSource` (the same
/// rasterize path as `DrawPrimitiveUP`, with `StartVertex` folding into the
/// stream base). An unset/invalid stream or a stride smaller than the FVF
/// layout is the honest `D3DERR_INVALIDCALL`.
pub fn handle_draw_primitive(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "DrawPrimitive")?;
    let primitive_type = read_arg(engine, ArgReg::Rdx, "DrawPrimitive")?;
    let start_vertex = read_arg(engine, ArgReg::R8, "DrawPrimitive")?;
    let primitive_count = read_arg(engine, ArgReg::R9, "DrawPrimitive")?;

    let return_value =
        draw_buffer_form_common(state, primitive_type, primitive_count, None, start_vertex)?;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::DrawIndexedPrimitive` (vtable slot 82) — the
/// buffer-form indexed draw.
///
/// L2: real — resolves triangle corners through the index buffer bound by
/// `SetIndices` against the vertex buffer bound by `SetStreamSource`.
/// `StartIndex` offsets into the index buffer and `BaseVertexIndex` (a
/// signed `INT`) shifts every resolved vertex position.
pub fn handle_draw_indexed_primitive(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "DrawIndexedPrimitive")?;
    let primitive_type = read_arg(engine, ArgReg::Rdx, "DrawIndexedPrimitive")?;
    let base_vertex_index = read_arg(engine, ArgReg::R8, "DrawIndexedPrimitive")?;
    let _min_vertex_index = read_arg(engine, ArgReg::R9, "DrawIndexedPrimitive")?;
    let _num_vertices = read_stack_argument(engine, 0x28, "DrawIndexedPrimitive NumVertices")?;
    let start_index = read_stack_argument(engine, 0x30, "DrawIndexedPrimitive StartIndex")?;
    let primitive_count = read_stack_argument(engine, 0x38, "DrawIndexedPrimitive PrimitiveCount")?;

    // The indexed form's index stream offsets into the index buffer, so the
    // vertex offset is not the start-vertex slot (which the non-indexed form
    // uses) — pass a per-draw index-stream marker instead.
    let return_value = draw_buffer_form_common(
        state,
        primitive_type,
        primitive_count,
        Some((base_vertex_index, start_index)),
        0,
    )?;

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetStreamSource` (vtable slot 100).
///
/// Stores stream 0's vertex buffer (object VA), `OffsetInBytes`, and stride.
/// A non-NULL stream data must be a known vertex buffer (the honest D3D9
/// contract — binding garbage must not silently draw nothing later).
pub fn handle_set_stream_source(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetStreamSource")?;
    let stream_number = read_arg(engine, ArgReg::Rdx, "SetStreamSource")?;
    let stream_data = read_arg(engine, ArgReg::R8, "SetStreamSource")?;
    let offset_in_bytes = read_arg(engine, ArgReg::R9, "SetStreamSource")?;
    let stride_raw = read_stack_argument(engine, 0x28, "SetStreamSource Stride")?;

    let known_buffer = stream_data == 0
        || state
            .d3d9()
            .d3d9_buffers
            .get(&stream_data)
            .is_some_and(|record| matches!(record.kind, BufferKind::Vertex { .. }));

    let return_value = if stream_number == 0 && known_buffer {
        state.d3d9().d3d9_stream_source_va = stream_data;
        state.d3d9().d3d9_stream_stride =
            u32::try_from(stride_raw & u64::from(u32::MAX)).unwrap_or(0);
        state.d3d9().d3d9_stream_offset =
            u32::try_from(offset_in_bytes & u64::from(u32::MAX)).unwrap_or(0);
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetStreamSource` (vtable slot 101).
///
/// Round-trip getter: writes back the stream-0 buffer, `OffsetInBytes`, and
/// stride. Streams beyond 0 were never bound (stream 1+ is out of slice 1),
/// so they read as a NULL buffer with zero offset/stride.
pub fn handle_get_stream_source(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetStreamSource")?;
    let stream_number = read_arg(engine, ArgReg::Rdx, "GetStreamSource")?;
    let pp_stream_data = read_arg(engine, ArgReg::R8, "GetStreamSource")?;
    let p_offset = read_arg(engine, ArgReg::R9, "GetStreamSource")?;
    let p_stride = read_stack_argument(engine, 0x28, "GetStreamSource pStride")?;

    let (stream_va, offset, stride) = if stream_number == 0 {
        let d3d = state.d3d9();
        (
            d3d.d3d9_stream_source_va,
            d3d.d3d9_stream_offset,
            d3d.d3d9_stream_stride,
        )
    } else {
        (0, 0, 0)
    };
    if pp_stream_data != 0 {
        write_guest_u64(engine, pp_stream_data, stream_va)
            .context("failed to write GetStreamSource buffer output")?;
    }
    // `pOffsetInBytes` / `pStride` are 4-byte `UINT` out-params — an 8-byte
    // write would clobber the guest's adjacent stack locals.
    if p_offset != 0 {
        write_guest_u32(engine, p_offset, offset)
            .context("failed to write GetStreamSource offset output")?;
    }
    if p_stride != 0 {
        write_guest_u32(engine, p_stride, stride)
            .context("failed to write GetStreamSource stride output")?;
    }

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::SetIndices` (vtable slot 104).
///
/// Binds the index buffer for the buffer-form indexed draws. A non-NULL
/// pointer must be a known index buffer (the honest D3D9 contract).
pub fn handle_set_indices(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetIndices")?;
    let index_data = read_arg(engine, ArgReg::Rdx, "SetIndices")?;

    let known_buffer = index_data == 0
        || state
            .d3d9()
            .d3d9_buffers
            .get(&index_data)
            .is_some_and(|record| matches!(record.kind, BufferKind::Index { .. }));

    let return_value = if known_buffer {
        state.d3d9().d3d9_index_buffer_va = index_data;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetIndices` (vtable slot 105).
///
/// Round-trip getter: writes back the index buffer bound by `SetIndices`.
pub fn handle_get_indices(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetIndices")?;
    let pp_index_data = read_arg(engine, ArgReg::Rdx, "GetIndices")?;

    if pp_index_data != 0 {
        write_guest_u64(engine, pp_index_data, state.d3d9().d3d9_index_buffer_va)
            .context("failed to write GetIndices output")?;
    }

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::CreateVertexBuffer` (vtable slot 26).
///
/// L2: real — allocates an `IDirect3DVertexBuffer9` object backed by a
/// host-owned byte store (see [`buffer`](super::buffer) for the lock model).
/// `Length` must be > 0, the FVF parseable, and the pool not SCRATCH; the
/// guest fills the buffer through `Lock`/`Unlock`, binds it with
/// `SetStreamSource`, and the buffer-form draws read the host copy.
pub fn handle_create_vertex_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    // The FVF must describe a vertex layout the rasterizer can decode; an
    // unparseable mask (no XYZ/XYZRHW, or both) is a real Create-time error.
    create_buffer_impl(ctx, "CreateVertexBuffer", D3d9Iface::VertexBuffer9, |fvf| {
        parse_fvf(fvf).is_some()
    })
}

/// Handles `IDirect3DDevice9::CreateIndexBuffer` (vtable slot 27).
///
/// L2: real — the index-buffer analogue of [`handle_create_vertex_buffer`].
/// The format must be `D3DFMT_INDEX16` (101) or `D3DFMT_INDEX32` (102); the
/// index size drives the buffer-form `DrawIndexedPrimitive` fetch.
pub fn handle_create_index_buffer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_buffer_impl(
        ctx,
        "CreateIndexBuffer",
        D3d9Iface::IndexBuffer9,
        |format| matches!(format, D3DFMT_INDEX16 | D3DFMT_INDEX32),
    )
}

/// Shared `CreateVertexBuffer`/`CreateIndexBuffer` body: read the arguments
/// (`rcx` = this, `rdx` = length, `r8` = usage, `r9` = FVF/format; pool,
/// output pointer and shared handle on the stack), validate via `validator`,
/// allocate the buffer object, and write back the interface pointer.
fn create_buffer_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    iface: D3d9Iface,
    validator: impl Fn(u32) -> bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, api_name)?;
    let length_raw = read_arg(engine, ArgReg::Rdx, api_name)?;
    let usage_raw = read_arg(engine, ArgReg::R8, api_name)?;
    let format_raw = read_arg(engine, ArgReg::R9, api_name)?;
    // Per-API stack-slot context names (kept byte-exact for the A/W-style
    // error strings the two entry points had before sharing this body).
    let (pool_arg, pp_buffer_arg, shared_handle_arg) = match iface {
        D3d9Iface::IndexBuffer9 => (
            "CreateIndexBuffer Pool",
            "CreateIndexBuffer ppBuffer",
            "CreateIndexBuffer pSharedHandle",
        ),
        _ => (
            "CreateVertexBuffer Pool",
            "CreateVertexBuffer ppBuffer",
            "CreateVertexBuffer pSharedHandle",
        ),
    };
    let pool_raw = read_stack_argument(engine, 0x28, pool_arg)?;
    let pp_buffer = read_stack_argument(engine, 0x30, pp_buffer_arg)?;
    let _shared_handle = read_stack_argument(engine, 0x38, shared_handle_arg)?;

    let length = low_u32(length_raw, "buffer length")?;
    let usage = low_u32(usage_raw, "buffer usage")?;
    let format = low_u32(format_raw, "buffer FVF/format")?;
    let pool = low_u32(pool_raw, "buffer pool")?;

    let iface_pointer = match iface {
        D3d9Iface::IndexBuffer9 => "IDirect3DIndexBuffer9",
        _ => "IDirect3DVertexBuffer9",
    };
    let return_value = if validator(format) && pp_buffer != 0 {
        let object = create_buffer_record(engine, state, iface, length, usage, pool, format)?;
        if object == 0 {
            write_guest_u64(engine, pp_buffer, 0)
                .with_context(|| format!("failed to clear {api_name} output pointer"))?;
            D3DERR_INVALIDCALL
        } else {
            write_guest_u64(engine, pp_buffer, object)
                .with_context(|| format!("failed to return {iface_pointer} pointer"))?;
            D3D_OK
        }
    } else {
        if pp_buffer != 0 {
            write_guest_u64(engine, pp_buffer, 0)
                .with_context(|| format!("failed to clear {api_name} output pointer"))?;
        }
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::CreateRenderTarget` (vtable slot 24).
///
/// L6: real — creates an offscreen render-target surface with its own host
/// texel buffer. Formats `D3DFMT_A8R8G8B8` (21) and `D3DFMT_X8R8G8B8` (22)
/// are accepted; a multisample request above `D3DMULTISAMPLE_NONE` (0) is the
/// honest `D3DERR_INVALIDCALL` (the software rasterizer is single-sampled).
/// The surface is a first-class `IDirect3DSurface9`: `GetDesc` describes it
/// and `LockRect`/`UnlockRect` read its texels back.
pub fn handle_create_render_target(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "CreateRenderTarget")?;
    let width_raw = read_arg(engine, ArgReg::Rdx, "CreateRenderTarget")?;
    let height_raw = read_arg(engine, ArgReg::R8, "CreateRenderTarget")?;
    let format_raw = read_arg(engine, ArgReg::R9, "CreateRenderTarget")?;
    let multi_sample = read_stack_argument(engine, 0x28, "CreateRenderTarget MultiSample")?;
    let _multi_sample_quality =
        read_stack_argument(engine, 0x30, "CreateRenderTarget MultiSampleQuality")?;
    let _lockable = read_stack_argument(engine, 0x38, "CreateRenderTarget Lockable")?;
    let pp_surface = read_stack_argument(engine, 0x40, "CreateRenderTarget ppSurface")?;
    let _shared_handle = read_stack_argument(engine, 0x48, "CreateRenderTarget pSharedHandle")?;

    let width = low_u32(width_raw, "CreateRenderTarget width")?;
    let height = low_u32(height_raw, "CreateRenderTarget height")?;
    let format = low_u32(format_raw, "CreateRenderTarget format")?;
    let multi_sample = low_u32(multi_sample, "CreateRenderTarget MultiSample")?;

    // The rasterizer is single-sampled; a multisample request beyond NONE
    // cannot be honored honestly.
    let valid = width > 0
        && height > 0
        && width <= MAX_TEXTURE_DIMENSION
        && height <= MAX_TEXTURE_DIMENSION
        && matches!(format, super::D3DFMT_A8R8G8B8 | super::D3DFMT_X8R8G8B8)
        && multi_sample == 0
        && pp_surface != 0;

    let return_value = if valid {
        let object = super::texture::allocate_surface_object(engine, state)?;
        if object == 0 {
            D3DERR_INVALIDCALL
        } else {
            let texel_count = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
            state.d3d9().d3d9_render_targets.insert(
                object,
                super::texture::RenderTargetRecord {
                    handle: object,
                    width,
                    height,
                    format,
                    pixels: std::sync::Arc::new(vec![0; texel_count]),
                    locked_va: 0,
                    locked_rect: None,
                },
            );
            write_guest_u64(engine, pp_surface, object)
                .context("failed to return render-target surface pointer")?;
            D3D_OK
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetRenderTarget` (vtable slot 36).
///
/// L6: real — binds render-target slot 0 to an offscreen surface created by
/// [`handle_create_render_target`]; `NULL` (0) rebinds the implicit
/// backbuffer. A non-`D3D9_RENDERTARGET`-bound surface is rejected with the
/// honest `D3DERR_INVALIDCALL`. Multi-target slots (1..3) are not modeled —
/// the same honest error, documented (the software rasterizer writes a single
/// color buffer).
pub fn handle_set_render_target(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetRenderTarget")?;
    let index_raw = read_arg(engine, ArgReg::Rdx, "SetRenderTarget")?;
    let surface = read_arg(engine, ArgReg::R8, "SetRenderTarget")?;

    let index = low_u32(index_raw, "SetRenderTarget index")?;
    // NULL rebinds the backbuffer; a non-NULL surface must be a known RT.
    let valid =
        index == 0 && (surface == 0 || state.d3d9().d3d9_render_targets.contains_key(&surface));
    let return_value = if valid {
        state.d3d9().d3d9_render_target = surface;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetRenderTarget` (vtable slot 37).
///
/// L6: real — returns the surface bound to render-target slot 0 (the
/// implicit backbuffer when none is bound: a NULL surface is returned and
/// `D3D_OK` reported, matching D3D9's "no RT bound" contract for the
/// backbuffer default).
pub fn handle_get_render_target(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetRenderTarget")?;
    let index_raw = read_arg(engine, ArgReg::Rdx, "GetRenderTarget")?;
    let pp_surface = read_arg(engine, ArgReg::R8, "GetRenderTarget")?;

    let index = low_u32(index_raw, "GetRenderTarget index")?;
    let return_value = if index == 0 && pp_surface != 0 {
        let bound = state.d3d9().d3d9_render_target;
        write_guest_u64(engine, pp_surface, bound)
            .context("failed to return render-target surface pointer")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}
