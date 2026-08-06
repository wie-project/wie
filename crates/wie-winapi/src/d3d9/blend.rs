use anyhow::{Context, Result};

use super::texture::allocate_surface_object;
use super::{
    D3D_OK, D3DERR_INVALIDCALL, D3DFMT_D16, D3DFMT_D24S8, DepthStencilRecord, read_stack_argument,
};
use crate::guest_memory::{write_u32 as write_guest_u32, write_u64 as write_guest_u64};
use crate::{HandlerContext, WinApiHandlerResult};

// ── blend + depth handlers ──────────────────────────────────────────────

/// Handles `IDirect3DDevice9::GetRenderState` (vtable slot 58).
///
/// Round-trip getter (L3): modeled states read back the typed value;
/// unmodeled states read the last-set raw value from the raw-value layer
/// (0 when never set) — D3D9's round-trip fidelity for every state.
pub fn handle_get_render_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetRenderState")?;
    let state_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetRenderState")?;
    let p_value = engine
        .read_r8()
        .context("failed to read R8 for GetRenderState")?;

    let state_id = u32::try_from(state_raw & u64::from(u32::MAX))
        .context("GetRenderState state identifier does not fit u32")?;
    if p_value != 0 {
        let d3d = state.d3d9();
        let value = match d3d.d3d9_render_state.value_of(state_id) {
            Some(v) => v,
            None => d3d
                .d3d9_render_state_raw
                .get(&state_id)
                .copied()
                .unwrap_or(0),
        };
        write_guest_u32(engine, p_value, value).context("failed to write GetRenderState output")?;
    }

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::CreateDepthStencilSurface` (vtable slot 29).
///
/// Formats: `D3DFMT_D16` (80) and `D3DFMT_D24S8` (75 — depth only, the stencil
/// bits are unused). The depth texels are host-owned `f32` in `0.0 = near`
/// .. `1.0 = far`, initialized to the far plane.
pub fn handle_create_depth_stencil_surface(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreateDepthStencilSurface")?;
    let width_raw = engine
        .read_rdx()
        .context("failed to read RDX for CreateDepthStencilSurface")?;
    let height_raw = engine
        .read_r8()
        .context("failed to read R8 for CreateDepthStencilSurface")?;
    let format_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateDepthStencilSurface")?;
    let _multi_sample = read_stack_argument(engine, 0x28, "CreateDepthStencilSurface MultiSample")?;
    let _multi_sample_quality =
        read_stack_argument(engine, 0x30, "CreateDepthStencilSurface MultiSampleQuality")?;
    let _discard = read_stack_argument(engine, 0x38, "CreateDepthStencilSurface Discard")?;
    let pp_surface = read_stack_argument(engine, 0x40, "CreateDepthStencilSurface ppSurface")?;
    let _shared_handle =
        read_stack_argument(engine, 0x48, "CreateDepthStencilSurface pSharedHandle")?;

    let width = u32::try_from(width_raw & u64::from(u32::MAX))
        .context("CreateDepthStencilSurface width does not fit u32")?;
    let height = u32::try_from(height_raw & u64::from(u32::MAX))
        .context("CreateDepthStencilSurface height does not fit u32")?;
    let format = u32::try_from(format_raw & u64::from(u32::MAX))
        .context("CreateDepthStencilSurface format does not fit u32")?;

    let valid = width > 0
        && height > 0
        && width <= 4096
        && height <= 4096
        && matches!(format, D3DFMT_D16 | D3DFMT_D24S8)
        && pp_surface != 0;

    let return_value = if valid {
        let object = allocate_surface_object(engine, state)?;
        if object == 0 {
            D3DERR_INVALIDCALL
        } else {
            let depth_count = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
            state.d3d9().d3d9_depth_surfaces.insert(
                object,
                DepthStencilRecord {
                    handle: object,
                    width,
                    height,
                    format,
                    depth: vec![1.0; depth_count],
                },
            );
            write_guest_u64(engine, pp_surface, object)
                .context("failed to return depth-stencil surface pointer")?;
            D3D_OK
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetDepthStencilSurface` (vtable slot 38).
pub fn handle_set_depth_stencil_surface(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetDepthStencilSurface")?;
    let surface = engine
        .read_rdx()
        .context("failed to read RDX for SetDepthStencilSurface")?;

    // NULL unbinds; a non-NULL surface must be a known depth surface.
    let valid = surface == 0 || state.d3d9().d3d9_depth_surfaces.contains_key(&surface);
    let return_value = if valid {
        state.d3d9().d3d9_depth_stencil = surface;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetDepthStencilSurface` (vtable slot 39).
pub fn handle_get_depth_stencil_surface(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetDepthStencilSurface")?;
    let pp_surface = engine
        .read_rdx()
        .context("failed to read RDX for GetDepthStencilSurface")?;

    let return_value = if pp_surface != 0 {
        let bound = state.d3d9().d3d9_depth_stencil;
        write_guest_u64(engine, pp_surface, bound)
            .context("failed to write GetDepthStencilSurface output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}
