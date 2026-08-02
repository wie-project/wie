//! Direct3D 9 API surface: `Direct3DCreate9`, the `IDirect3D9` adapter
//! queries, `CreateDevice`, and the `IDirect3DDevice9` dispatch handlers
//! (render state, scene, viewport, streams, index/vertex buffers).
//!
//! Shader objects live in [`shader`], software rasterization in [`raster`],
//! texture/surface lifecycle in [`texture`], and blend/depth state in
//! [`blend`].

use anyhow::{Context, Result};

use crate::d3d9_render::{
    D3DRS_ALPHABLENDENABLE, D3DRS_BLENDOP, D3DRS_DESTBLEND, D3DRS_SRCBLEND, D3DRS_ZENABLE,
    D3DRS_ZFUNC, D3DRS_ZWRITEENABLE, D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER,
    D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER, D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP,
    D3DTSS_COLORARG1, D3DTSS_COLORARG2, D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX, D3dBlend, D3dBlendOp,
    D3dCmpFunc, D3dZBufferType, RenderState, TextureStageState, parse_fvf,
};
use crate::fake_va::{
    D3d9Iface, Device9Method, Direct3D9Method, Surface9Method, Texture9Method, encode_com,
};
use crate::guest_memory::{
    read_u32 as read_guest_u32, read_u64 as read_guest_u64, write_u32 as write_guest_u32,
    write_u64 as write_guest_u64,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

mod blend;
mod raster;
mod shader;
mod texture;

use raster::{
    draw_vertex_stream, fill_backbuffer_rect, parse_mat4, primitive_vertex_count, read_f32_at,
    read_u32_at,
};

pub use blend::{
    handle_create_depth_stencil_surface, handle_get_depth_stencil_surface, handle_get_render_state,
    handle_set_depth_stencil_surface,
};
pub use shader::{
    IDIRECT3DSHADER9_METHOD_COUNT, handle_create_pixel_shader, handle_create_vertex_shader,
    handle_get_pixel_shader, handle_get_pixel_shader_constant_f, handle_get_vertex_shader,
    handle_get_vertex_shader_constant_f, handle_pixel_shader_release, handle_set_pixel_shader,
    handle_set_pixel_shader_constant_f, handle_set_vertex_shader,
    handle_set_vertex_shader_constant_f, handle_vertex_shader_release,
};
pub use texture::{
    DepthStencilRecord, TextureRecord, handle_create_texture, handle_get_sampler_state,
    handle_get_texture, handle_get_texture_stage_state, handle_set_texture,
    handle_surface_lock_rect, handle_surface_release, handle_surface_unlock_rect,
    handle_texture_get_level_count, handle_texture_get_surface_level, handle_texture_lock_rect,
    handle_texture_release, handle_texture_unlock_rect,
};

/// Expected `D3D_SDK_VERSION` for Direct3D 9.
const D3D_SDK_VERSION: u64 = 32;

/// Size reserved for one fake `IDirect3D9` vtable and object.
const IDIRECT3D9_ALLOCATION_SIZE: u64 = 0x100;

/// Offset of the COM object after its vtable.
const IDIRECT3D9_OBJECT_OFFSET: u64 = 0x90;

/// Fake target VA for `IDirect3D9` vtable slot `slot`.
#[must_use]
pub fn idirect3d9_method_va(slot: usize) -> u64 {
    let method = u8::try_from(slot).unwrap_or(u8::MAX);
    encode_com(D3d9Iface::Direct3D9, method)
}

const FAKE_MONITOR_HANDLE: u64 = 0x0000_0000_6600_0010;

pub(crate) const D3D_OK: u64 = 0;
pub(crate) const D3DERR_INVALIDCALL: u64 = 0x8876_086c;

/// `D3DPS_VERSION(2, 0)` — the pixel-shader version reported in D3DCAPS9.
const D3DPS_VERSION_2_0: u32 = 0xFFFF_0200;

const D3DDEVTYPE_HAL: u64 = 1;
const D3DDEVTYPE_REF: u64 = 2;
const D3DDEVTYPE_SW: u64 = 3;

const D3DCAPS9_SIZE: usize = 304;

// P3 caps honesty (B6c): slice 1 has NO programmable pipeline. The raw
// D3DCAPS9 is zeroed first, so the VertexShaderVersion / PixelShaderVersion
// fields read 0 (a device without shader support) — games branching on
// `caps.XxxShaderVersion >= D3D*_VERSION(1,0)` take the fixed-function path
// we actually implement, instead of silently taking an unsupported one.

pub(crate) const D3DFMT_X8R8G8B8: u32 = 22;

/// `D3DCLEAR_TARGET` — clear the render-target (backbuffer) surface.
const D3DCLEAR_TARGET: u32 = 0x0000_0001;
/// `D3DCLEAR_ZBUFFER` — clear the depth buffer (deferred in slice 1).
const D3DCLEAR_ZBUFFER: u32 = 0x0000_0002;

/// `D3DTS_WORLD` (world matrix index 0).
const D3DTS_WORLD: u32 = 256;
/// `D3DTS_VIEW`.
const D3DTS_VIEW: u32 = 2;
/// `D3DTS_PROJECTION`.
const D3DTS_PROJECTION: u32 = 3;

/// `D3DFMT_INDEX32` — 32-bit indices (102). 16-bit indices (`D3DFMT_INDEX16`,
/// 101) are the default and need no constant here.
pub(crate) const D3DFMT_INDEX32: u32 = 102;

/// `D3DFMT_A8R8G8B8` — 32-bpp with alpha (texels stored `0xAARRGGBB`).
pub(crate) const D3DFMT_A8R8G8B8: u32 = 21;

/// `D3DFMT_D24S8` — 32-bpp depth+stencil (75; stencil bits unused in P4c).
pub(crate) const D3DFMT_D24S8: u32 = 75;
/// `D3DFMT_D16` — 16-bpp depth (80).
pub(crate) const D3DFMT_D16: u32 = 80;

/// Number of methods in the `IDirect3DTexture9` vtable (0..21).
pub const IDIRECT3DTEXTURE9_METHOD_COUNT: usize = Texture9Method::VTABLE_SLOTS;
/// Space reserved for the texture vtable + COM object.
pub(crate) const IDIRECT3DTEXTURE9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 22-entry vtable.
pub(crate) const IDIRECT3DTEXTURE9_OBJECT_OFFSET: u64 = 0x80;

/// Number of methods in the `IDirect3DSurface9` vtable (0..17).
pub const IDIRECT3DSURFACE9_METHOD_COUNT: usize = Surface9Method::VTABLE_SLOTS;
/// Space reserved for the surface vtable + COM object.
pub(crate) const IDIRECT3DSURFACE9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 18-entry vtable.
pub(crate) const IDIRECT3DSURFACE9_OBJECT_OFFSET: u64 = 0x80;

/// Number of methods in the `IDirect3DDevice9` vtable.
pub const IDIRECT3DDEVICE9_METHOD_COUNT: usize = Device9Method::VTABLE_SLOTS;

/// Space reserved for the device vtable and COM object.
const IDIRECT3DDEVICE9_ALLOCATION_SIZE: u64 = 0x400;

/// Offset of the COM object after its 119-entry vtable.
const IDIRECT3DDEVICE9_OBJECT_OFFSET: u64 = 0x3c0;

const D3DCREATE_SOFTWARE_VERTEXPROCESSING: u32 = 0x0000_0020;
const D3DCREATE_HARDWARE_VERTEXPROCESSING: u32 = 0x0000_0040;
const D3DCREATE_MIXED_VERTEXPROCESSING: u32 = 0x0000_0080;

/// Fake target VA for `IDirect3DDevice9` vtable slot `slot`.
pub fn idirect3ddevice9_method_va(slot: usize) -> Result<u64> {
    if slot >= IDIRECT3DDEVICE9_METHOD_COUNT {
        anyhow::bail!("IDirect3DDevice9 method slot {slot} out of range");
    }
    let method = u8::try_from(slot).context("IDirect3DDevice9 slot does not fit u8")?;
    Ok(encode_com(D3d9Iface::Device9, method))
}

/// Handles dynamically resolved `D3D9.dll!Direct3DCreate9`.
pub fn handle_direct3d_create9(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let sdk_version = engine
        .read_rcx()
        .context("failed to read RCX for Direct3DCreate9")?;

    let return_value = if sdk_version == D3D_SDK_VERSION {
        let vtable_address =
            allocate_direct3d_block(engine, state, IDIRECT3D9_ALLOCATION_SIZE, "IDirect3D9");

        if vtable_address == 0 {
            0
        } else {
            for slot in 0..Direct3D9Method::VTABLE_SLOTS {
                let slot_u64 =
                    u64::try_from(slot).context("IDirect3D9 vtable slot does not fit u64")?;

                let byte_offset = slot_u64
                    .checked_mul(8)
                    .context("IDirect3D9 vtable offset overflow")?;

                let entry_address = vtable_address
                    .checked_add(byte_offset)
                    .context("IDirect3D9 vtable entry address overflow")?;

                write_guest_u64(engine, entry_address, idirect3d9_method_va(slot))?;
            }

            let object_address = vtable_address
                .checked_add(IDIRECT3D9_OBJECT_OFFSET)
                .context("IDirect3D9 object address overflow")?;

            // A COM object starts with a pointer to its vtable.
            write_guest_u64(engine, object_address, vtable_address)?;

            state.d3d9().d3d9_object_address = object_address;
            state.d3d9().d3d9_ref_count = 1;

            object_address
        }
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from Direct3DCreate9")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3D9::GetAdapterCount`.
pub fn handle_get_adapter_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::GetAdapterCount")?;

    // Expose one deterministic display adapter.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::GetAdapterCount")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3D9::GetAdapterMonitor`.
pub fn handle_get_adapter_monitor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::GetAdapterMonitor")?;

    let adapter = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3D9::GetAdapterMonitor")?;

    let return_value = if adapter == 0 { FAKE_MONITOR_HANDLE } else { 0 };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::GetAdapterMonitor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

fn write_caps_u32(
    engine: &mut dyn wie_cpu::CpuEngine,
    caps_address: u64,
    offset: u64,
    value: u32,
    field_name: &str,
) -> Result<()> {
    let field_address = caps_address
        .checked_add(offset)
        .with_context(|| format!("D3DCAPS9 field address overflow: {field_name}"))?;

    write_guest_u32(engine, field_address, value)
        .with_context(|| format!("failed to write D3DCAPS9 field: {field_name}"))
}

/// Handles `IDirect3D9::GetDeviceCaps`.
pub fn handle_get_device_caps(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::GetDeviceCaps")?;

    let adapter = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3D9::GetDeviceCaps")?;

    let device_type = engine
        .read_r8()
        .context("failed to read R8 for IDirect3D9::GetDeviceCaps")?;

    let caps_address = engine
        .read_r9()
        .context("failed to read R9 for IDirect3D9::GetDeviceCaps")?;

    let valid_device_type = matches!(device_type, D3DDEVTYPE_HAL | D3DDEVTYPE_REF | D3DDEVTYPE_SW);

    let return_value = if adapter != 0 || !valid_device_type || caps_address == 0 {
        D3DERR_INVALIDCALL
    } else {
        let zeroed_caps = [0_u8; D3DCAPS9_SIZE];

        engine
            .mem_write(caps_address, &zeroed_caps)
            .context("failed to clear D3DCAPS9 structure")?;

        let device_type_u32 =
            u32::try_from(device_type).context("D3D device type does not fit u32")?;

        // D3DDEVTYPE DeviceType
        write_caps_u32(engine, caps_address, 0, device_type_u32, "DeviceType")?;

        // UINT AdapterOrdinal
        write_caps_u32(engine, caps_address, 4, 0, "AdapterOrdinal")?;

        // MaxTextureWidth / MaxTextureHeight
        write_caps_u32(engine, caps_address, 88, 4096, "MaxTextureWidth")?;
        write_caps_u32(engine, caps_address, 92, 4096, "MaxTextureHeight")?;

        // TextureCaps — P4b: the sampler handles non-power-of-two sizes with
        // wrap/clamp address modes, so claim NONPOW2CONDITIONAL (0x100).
        // POW2 is trivially a subset and needs no separate claim.
        write_caps_u32(engine, caps_address, 60, 0x0000_0100, "TextureCaps")?;

        // MaxVolumeExtent
        write_caps_u32(engine, caps_address, 96, 256, "MaxVolumeExtent")?;

        // MaxTextureRepeat
        write_caps_u32(engine, caps_address, 100, 8192, "MaxTextureRepeat")?;

        // MaxTextureAspectRatio
        write_caps_u32(engine, caps_address, 104, 4096, "MaxTextureAspectRatio")?;

        // MaxAnisotropy
        write_caps_u32(engine, caps_address, 108, 1, "MaxAnisotropy")?;

        // MaxVertexW = 1.0f
        write_caps_u32(engine, caps_address, 112, 1.0_f32.to_bits(), "MaxVertexW")?;

        // MaxTextureBlendStages
        write_caps_u32(engine, caps_address, 148, 8, "MaxTextureBlendStages")?;

        // MaxSimultaneousTextures
        write_caps_u32(engine, caps_address, 152, 8, "MaxSimultaneousTextures")?;

        // MaxActiveLights
        write_caps_u32(engine, caps_address, 160, 8, "MaxActiveLights")?;

        // MaxUserClipPlanes
        write_caps_u32(engine, caps_address, 164, 6, "MaxUserClipPlanes")?;

        // MaxVertexBlendMatrices
        write_caps_u32(engine, caps_address, 168, 4, "MaxVertexBlendMatrices")?;

        // MaxPointSize = 64.0f
        write_caps_u32(
            engine,
            caps_address,
            176,
            64.0_f32.to_bits(),
            "MaxPointSize",
        )?;

        // MaxPrimitiveCount
        write_caps_u32(engine, caps_address, 180, 1_048_575, "MaxPrimitiveCount")?;

        // MaxVertexIndex
        write_caps_u32(engine, caps_address, 184, 1_048_575, "MaxVertexIndex")?;

        // MaxStreams
        write_caps_u32(engine, caps_address, 188, 16, "MaxStreams")?;

        // MaxStreamStride
        write_caps_u32(engine, caps_address, 192, 255, "MaxStreamStride")?;

        // VertexShaderVersion — P5a caps honesty: the vertex stage is still
        // the FFP Gouraud path (vs execution is P5a-2), so this stays 0 —
        // games branching on `VertexShaderVersion != 0` take the FFP path
        // they actually get.

        // MaxVertexShaderConst — the vs_2_0 constant file is implemented
        // (256 float4s; used when P5a-2 executes vertex shaders).
        write_caps_u32(engine, caps_address, 200, 256, "MaxVertexShaderConst")?;

        // PixelShaderVersion — P5a: the ps_2_0 interpreter runs in the
        // fragment stage, so the caps honestly report D3DPS_VERSION(2,0) =
        // 0xFFFF0200. (PS20Caps stays zeroed — games gate on the version.)
        write_caps_u32(
            engine,
            caps_address,
            204,
            D3DPS_VERSION_2_0,
            "PixelShaderVersion",
        )?;

        // PixelShader1xMaxValue — the ps_1_x color scale (1.0 for ps_2_0).
        write_caps_u32(
            engine,
            caps_address,
            208,
            1.0_f32.to_bits(),
            "PixelShader1xMaxValue",
        )?;

        // MasterAdapterOrdinal
        write_caps_u32(engine, caps_address, 224, 0, "MasterAdapterOrdinal")?;

        // AdapterOrdinalInGroup
        write_caps_u32(engine, caps_address, 228, 0, "AdapterOrdinalInGroup")?;

        // NumberOfAdaptersInGroup
        write_caps_u32(engine, caps_address, 232, 1, "NumberOfAdaptersInGroup")?;

        // NumSimultaneousRTs
        write_caps_u32(engine, caps_address, 240, 4, "NumSimultaneousRTs")?;

        D3D_OK
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::GetDeviceCaps")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3D9::GetAdapterDisplayMode`.
pub fn handle_get_adapter_display_mode(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::GetAdapterDisplayMode")?;

    let adapter = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3D9::GetAdapterDisplayMode")?;

    let display_mode_address = engine
        .read_r8()
        .context("failed to read R8 for IDirect3D9::GetAdapterDisplayMode")?;

    let return_value = if adapter != 0 || display_mode_address == 0 {
        D3DERR_INVALIDCALL
    } else {
        let width = u32::try_from(state.window_state().window_width)
            .context("D3D display width is negative or does not fit u32")?;

        let height = u32::try_from(state.window_state().window_height)
            .context("D3D display height is negative or does not fit u32")?;

        write_guest_u32(engine, display_mode_address, width)
            .context("failed to write D3DDISPLAYMODE.Width")?;

        let height_address = display_mode_address
            .checked_add(4)
            .context("D3DDISPLAYMODE.Height address overflow")?;

        write_guest_u32(engine, height_address, height)
            .context("failed to write D3DDISPLAYMODE.Height")?;

        let refresh_rate_address = display_mode_address
            .checked_add(8)
            .context("D3DDISPLAYMODE.RefreshRate address overflow")?;

        write_guest_u32(engine, refresh_rate_address, 60)
            .context("failed to write D3DDISPLAYMODE.RefreshRate")?;

        let format_address = display_mode_address
            .checked_add(12)
            .context("D3DDISPLAYMODE.Format address overflow")?;

        write_guest_u32(engine, format_address, D3DFMT_X8R8G8B8)
            .context("failed to write D3DDISPLAYMODE.Format")?;

        D3D_OK
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::GetAdapterDisplayMode")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

pub(crate) fn allocate_direct3d_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
    _allocation_name: &str,
) -> u64 {
    state.heap_state.heap.alloc_coherent(engine, size)
}

pub(crate) fn read_stack_argument(
    engine: &mut dyn wie_cpu::CpuEngine,
    offset: u64,
    argument_name: &str,
) -> Result<u64> {
    let stack_pointer = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {argument_name}"))?;

    let argument_address = stack_pointer
        .checked_add(offset)
        .with_context(|| format!("{argument_name} stack address overflow"))?;

    read_guest_u64(engine, argument_address)
}

fn normalize_presentation_parameters(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    parameters_address: u64,
    focus_window: u64,
) -> Result<()> {
    let width =
        read_guest_u32(engine, parameters_address).context("failed to read BackBufferWidth")?;

    let height_address = parameters_address
        .checked_add(4)
        .context("BackBufferHeight address overflow")?;

    let height =
        read_guest_u32(engine, height_address).context("failed to read BackBufferHeight")?;

    if width == 0 {
        let fallback_width = u32::try_from(state.window_state().window_width)
            .context("window width is negative or does not fit u32")?;

        write_guest_u32(engine, parameters_address, fallback_width)
            .context("failed to write fallback BackBufferWidth")?;
    }

    if height == 0 {
        let fallback_height = u32::try_from(state.window_state().window_height)
            .context("window height is negative or does not fit u32")?;

        write_guest_u32(engine, height_address, fallback_height)
            .context("failed to write fallback BackBufferHeight")?;
    }

    // On Win64, hDeviceWindow is at offset 32 because HWND is 64-bit aligned.
    let device_window_address = parameters_address
        .checked_add(32)
        .context("hDeviceWindow address overflow")?;

    let device_window =
        read_guest_u64(engine, device_window_address).context("failed to read hDeviceWindow")?;

    if device_window == 0 && focus_window != 0 {
        write_guest_u64(engine, device_window_address, focus_window)
            .context("failed to write fallback hDeviceWindow")?;
    }

    Ok(())
}

/// P5c: D3D9 render-resolution divisor from `WIE_D3D9_SCALE`.
///
/// A power of two (default 1; 4 = quarter-scale). Values that don't parse or
/// aren't powers of two fall back to 1; values above 16 are clamped (a
/// 320×240 backbuffer at scale 32 would be 10×7 — useless even for a
/// placeholder).
fn d3d9_render_scale() -> u32 {
    const MAX_SCALE: u32 = 16;
    std::env::var("WIE_D3D9_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(1, |s| s.next_power_of_two().clamp(1, MAX_SCALE))
}

/// Initialize the P3 software-render device state from the (already
/// normalized) presentation parameters: backbuffer size, present window, and
/// the default identity transforms + full-backbuffer viewport.
fn init_device_state(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    parameters_address: u64,
) -> Result<()> {
    let d3d = state.d3d9();
    let width =
        read_guest_u32(engine, parameters_address).context("failed to read BackBufferWidth")?;
    let height_address = parameters_address
        .checked_add(4)
        .context("BackBufferHeight address overflow")?;
    let height =
        read_guest_u32(engine, height_address).context("failed to read BackBufferHeight")?;

    // On Win64, hDeviceWindow is at offset 32 (HWND is 64-bit aligned).
    let device_window_address = parameters_address
        .checked_add(32)
        .context("hDeviceWindow address overflow")?;
    let present_hwnd =
        read_guest_u64(engine, device_window_address).context("failed to read hDeviceWindow")?;

    // P5c quarter-scale: `WIE_D3D9_SCALE` divides the render resolution by a
    // power of two (4 = quarter-scale). The guest still thinks it rendered at
    // the presentation size; the published frame is `w/scale × h/scale` and
    // the wgpu blit shader nearest-upscales it to the full window — a cheap
    // software-rendering speedup (16× fewer fragments at scale 4) at the cost
    // of blocky output, matching the roadmap's quarter-scale milestone.
    let scale = d3d9_render_scale();
    let width = width
        .checked_div(scale)
        .filter(|&w| w != 0)
        .unwrap_or(width.max(1));
    let height = height
        .checked_div(scale)
        .filter(|&h| h != 0)
        .unwrap_or(height.max(1));

    d3d.d3d9_backbuffer_width = width;
    d3d.d3d9_backbuffer_height = height;
    d3d.d3d9_present_hwnd = crate::handles::Hwnd::from(present_hwnd);
    d3d.d3d9_scene_active = false;
    d3d.d3d9_world_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_view_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_projection_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_viewport = (0, 0, width, height, 0.0, 1.0);
    d3d.d3d9_dirty = None;
    d3d.d3d9_stream_source_va = 0;
    d3d.d3d9_stream_stride = 0;
    d3d.d3d9_index_buffer_va = 0;

    let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
    d3d.d3d9_backbuffer = vec![0; needed];
    Ok(())
}

/// Handles `IDirect3D9::CreateDevice`.
pub fn handle_create_device(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3D9::CreateDevice")?;

    let adapter = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3D9::CreateDevice")?;

    let device_type = engine
        .read_r8()
        .context("failed to read R8 for IDirect3D9::CreateDevice")?;

    let focus_window = engine
        .read_r9()
        .context("failed to read R9 for IDirect3D9::CreateDevice")?;

    let behavior_flags_raw =
        read_stack_argument(engine, 0x28, "IDirect3D9::CreateDevice BehaviorFlags")?;

    let presentation_parameters_address = read_stack_argument(
        engine,
        0x30,
        "IDirect3D9::CreateDevice pPresentationParameters",
    )?;

    let returned_device_address = read_stack_argument(
        engine,
        0x38,
        "IDirect3D9::CreateDevice ppReturnedDeviceInterface",
    )?;

    let behavior_flags_low = behavior_flags_raw & u64::from(u32::MAX);

    let behavior_flags =
        u32::try_from(behavior_flags_low).context("CreateDevice behavior flags do not fit u32")?;

    let valid_device_type = matches!(device_type, D3DDEVTYPE_HAL | D3DDEVTYPE_REF | D3DDEVTYPE_SW);

    let vertex_processing_flags = D3DCREATE_SOFTWARE_VERTEXPROCESSING
        | D3DCREATE_HARDWARE_VERTEXPROCESSING
        | D3DCREATE_MIXED_VERTEXPROCESSING;

    let has_vertex_processing_flag = behavior_flags & vertex_processing_flags != 0;

    let valid_arguments = adapter == 0
        && valid_device_type
        && has_vertex_processing_flag
        && presentation_parameters_address != 0
        && returned_device_address != 0;

    let return_value = if valid_arguments {
        normalize_presentation_parameters(
            engine,
            state,
            presentation_parameters_address,
            focus_window,
        )?;

        init_device_state(engine, state, presentation_parameters_address)?;

        let vtable_address = allocate_direct3d_block(
            engine,
            state,
            IDIRECT3DDEVICE9_ALLOCATION_SIZE,
            "IDirect3DDevice9",
        );

        if vtable_address == 0 {
            write_guest_u64(engine, returned_device_address, 0)
                .context("failed to clear returned IDirect3DDevice9 pointer")?;

            D3DERR_INVALIDCALL
        } else {
            for slot in 0..IDIRECT3DDEVICE9_METHOD_COUNT {
                let slot_u64 =
                    u64::try_from(slot).context("IDirect3DDevice9 vtable slot does not fit u64")?;

                let vtable_offset = slot_u64
                    .checked_mul(8)
                    .context("IDirect3DDevice9 vtable offset overflow")?;

                let entry_address = vtable_address
                    .checked_add(vtable_offset)
                    .context("IDirect3DDevice9 vtable entry address overflow")?;

                let method_address = idirect3ddevice9_method_va(slot)?;

                write_guest_u64(engine, entry_address, method_address).with_context(|| {
                    format!("failed to write IDirect3DDevice9 vtable slot {slot}")
                })?;
            }

            let object_address = vtable_address
                .checked_add(IDIRECT3DDEVICE9_OBJECT_OFFSET)
                .context("IDirect3DDevice9 object address overflow")?;

            write_guest_u64(engine, object_address, vtable_address)
                .context("failed to initialize IDirect3DDevice9 object")?;

            write_guest_u64(engine, returned_device_address, object_address)
                .context("failed to return IDirect3DDevice9 pointer")?;

            state.d3d9().d3d9_device_object_address = object_address;
            state.d3d9().d3d9_device_ref_count = 1;

            D3D_OK
        }
    } else {
        if returned_device_address != 0 {
            write_guest_u64(engine, returned_device_address, 0)
                .context("failed to clear invalid IDirect3DDevice9 output pointer")?;
        }

        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3D9::CreateDevice")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
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
