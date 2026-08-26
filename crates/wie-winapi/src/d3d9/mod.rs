//! Direct3D 9 API surface: `Direct3DCreate9`, the `IDirect3D9` adapter
//! queries, `CreateDevice`, and the `IDirect3DDevice9` vtable / object
//! allocation.
//!
//! The `IDirect3DDevice9` dispatch handlers (render state, scene, viewport,
//! draws, streams, index/vertex buffers) live in [`device`]; shader objects
//! live in [`shader`], software rasterization in [`raster`], texture/surface
//! lifecycle in [`texture`], vertex/index-buffer COM objects in [`buffer`],
//! and blend/depth state in [`blend`].

use crate::gdi32::{ArgReg, read_arg};
use anyhow::{Context, Result};

use crate::fake_va::{
    D3d9Iface, Device9Method, Direct3D9Method, IndexBuffer9Method, Surface9Method, Texture9Method,
    VertexBuffer9Method, encode_com,
};
use crate::guest_memory::{
    read_u32, read_u64, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

mod blend;
mod buffer;
mod device;
mod draw;
mod raster;
mod shader;
mod texture;

pub use blend::{
    handle_create_depth_stencil_surface, handle_get_depth_stencil_surface, handle_get_render_state,
    handle_set_depth_stencil_surface,
};
pub use buffer::{
    BufferKind, BufferRecord, handle_index_buffer_add_ref, handle_index_buffer_get_desc,
    handle_index_buffer_lock, handle_index_buffer_query_interface, handle_index_buffer_release,
    handle_index_buffer_unlock, handle_vertex_buffer_add_ref, handle_vertex_buffer_get_desc,
    handle_vertex_buffer_lock, handle_vertex_buffer_query_interface, handle_vertex_buffer_release,
    handle_vertex_buffer_unlock,
};
pub use device::{
    handle_begin_scene, handle_clear, handle_create_index_buffer, handle_create_render_target,
    handle_create_vertex_buffer, handle_device_release, handle_direct3d9_release,
    handle_draw_indexed_primitive, handle_draw_indexed_primitive_up, handle_draw_primitive,
    handle_draw_primitive_up, handle_end_scene, handle_get_indices, handle_get_render_target,
    handle_get_stream_source, handle_get_transform, handle_get_viewport, handle_multiply_transform,
    handle_present, handle_set_fvf, handle_set_indices, handle_set_render_state,
    handle_set_render_target, handle_set_sampler_state, handle_set_scissor_rect,
    handle_set_stream_source, handle_set_texture_stage_state, handle_set_transform,
    handle_set_viewport,
};
pub use shader::{
    IDIRECT3DSHADER9_METHOD_COUNT, handle_create_pixel_shader, handle_create_vertex_shader,
    handle_get_pixel_shader, handle_get_pixel_shader_constant_f, handle_get_vertex_shader,
    handle_get_vertex_shader_constant_b, handle_get_vertex_shader_constant_f,
    handle_get_vertex_shader_constant_i, handle_pixel_shader_release, handle_set_pixel_shader,
    handle_set_pixel_shader_constant_f, handle_set_vertex_shader,
    handle_set_vertex_shader_constant_b, handle_set_vertex_shader_constant_f,
    handle_set_vertex_shader_constant_i, handle_vertex_shader_release,
};
pub use texture::{
    DepthStencilRecord, RenderTargetRecord, TextureRecord, handle_create_texture,
    handle_get_sampler_state, handle_get_texture, handle_get_texture_stage_state,
    handle_set_texture, handle_surface_get_desc, handle_surface_lock_rect, handle_surface_release,
    handle_surface_unlock_rect, handle_texture_get_level_count, handle_texture_get_surface_level,
    handle_texture_lock_rect, handle_texture_release, handle_texture_unlock_rect,
};

/// Expected `D3D_SDK_VERSION` for Direct3D 9.
const D3D_SDK_VERSION: u64 = 32;

/// Size reserved for one fake `IDirect3D9` vtable and object.
const IDIRECT3D9_ALLOCATION_SIZE: u64 = 0x100;

/// Offset of the COM object after its vtable.
pub(super) const IDIRECT3D9_OBJECT_OFFSET: u64 = 0x90;

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
/// `D3DVS_VERSION(2, 0)` — the vertex-shader version reported in D3DCAPS9
/// (the vs tag is `0xFFFE0000`, unlike the ps tag's `0xFFFF0000`).
const D3DVS_VERSION_2_0: u32 = 0xFFFE_0200;

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

/// `D3DPTEXTURECAPS_NONPOW2CONDITIONAL` — the `D3DCAPS9.TextureCaps` flag
/// claiming non-power-of-two texture support (conditional; the sampler handles
/// NPOT sizes with wrap/clamp address modes).
const D3DPTEXTURECAPS_NONPOW2CONDITIONAL: u32 = 0x0000_0100;

/// Maximum texture dimension the device accepts — the `D3DCAPS9`
/// MaxTextureWidth/Height/AspectRatio claim, enforced by the Create* checks.
pub(crate) const MAX_TEXTURE_DIMENSION: u32 = 4096;

/// `D3DCLEAR_TARGET` — clear the render-target (backbuffer) surface.
pub(super) const D3DCLEAR_TARGET: u32 = 0x0000_0001;
/// `D3DCLEAR_ZBUFFER` — clear the depth buffer (deferred in slice 1).
pub(super) const D3DCLEAR_ZBUFFER: u32 = 0x0000_0002;

/// `D3DTS_WORLD` (world matrix index 0).
pub(super) const D3DTS_WORLD: u32 = 256;
/// `D3DTS_VIEW`.
pub(super) const D3DTS_VIEW: u32 = 2;
/// `D3DTS_PROJECTION`.
pub(super) const D3DTS_PROJECTION: u32 = 3;

/// `D3DFMT_INDEX32` — 32-bit indices (102). 16-bit indices (`D3DFMT_INDEX16`,
/// 101) are the default and need no constant here.
pub(crate) const D3DFMT_INDEX32: u32 = 102;
/// `D3DFMT_INDEX16` — 16-bit indices (101).
pub(crate) const D3DFMT_INDEX16: u32 = 101;

/// `D3DFMT_A8R8G8B8` — 32-bpp with alpha (texels stored `0xAARRGGBB`).
pub(crate) const D3DFMT_A8R8G8B8: u32 = 21;

/// `D3DFMT_D24S8` — 32-bpp depth+stencil (75; stencil bits unused).
pub(crate) const D3DFMT_D24S8: u32 = 75;
/// `D3DFMT_D16` — 16-bpp depth (80).
pub(crate) const D3DFMT_D16: u32 = 80;

/// Number of methods in the `IDirect3DTexture9` vtable (0..21).
pub const IDIRECT3DTEXTURE9_METHOD_COUNT: usize = Texture9Method::VTABLE_SLOTS;
/// Space reserved for the texture vtable + COM object.
pub(crate) const IDIRECT3DTEXTURE9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 22-entry vtable.
pub(crate) const IDIRECT3DTEXTURE9_OBJECT_OFFSET: u64 = 0x80;

/// Number of methods in the `IDirect3DVertexBuffer9` vtable (0..13).
pub const IDIRECT3DVERTEXBUFFER9_METHOD_COUNT: usize = VertexBuffer9Method::VTABLE_SLOTS;
/// Space reserved for the vertex-buffer vtable + COM object.
pub(crate) const IDIRECT3DVERTEXBUFFER9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 14-entry vtable.
pub(crate) const IDIRECT3DVERTEXBUFFER9_OBJECT_OFFSET: u64 = 0x80;

/// Number of methods in the `IDirect3DIndexBuffer9` vtable (0..13).
pub const IDIRECT3DINDEXBUFFER9_METHOD_COUNT: usize = IndexBuffer9Method::VTABLE_SLOTS;
/// Space reserved for the index-buffer vtable + COM object.
pub(crate) const IDIRECT3DINDEXBUFFER9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 14-entry vtable.
pub(crate) const IDIRECT3DINDEXBUFFER9_OBJECT_OFFSET: u64 = 0x80;

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
pub(super) const IDIRECT3DDEVICE9_OBJECT_OFFSET: u64 = 0x3c0;

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
    let sdk_version = read_arg(engine, ArgReg::Rcx, "Direct3DCreate9")?;

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

    ctx.finish(return_value)
}

/// Handles `IDirect3D9::GetAdapterCount`.
pub fn handle_get_adapter_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::GetAdapterCount")?;

    // Expose one deterministic display adapter.
    let return_value = 1;

    ctx.finish(return_value)
}

/// Handles `IDirect3D9::GetAdapterMonitor`.
pub fn handle_get_adapter_monitor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::GetAdapterMonitor")?;

    let adapter = read_arg(engine, ArgReg::Rdx, "IDirect3D9::GetAdapterMonitor")?;

    let return_value = if adapter == 0 { FAKE_MONITOR_HANDLE } else { 0 };

    ctx.finish(return_value)
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
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::GetDeviceCaps")?;

    let adapter = read_arg(engine, ArgReg::Rdx, "IDirect3D9::GetDeviceCaps")?;

    let device_type = read_arg(engine, ArgReg::R8, "IDirect3D9::GetDeviceCaps")?;

    let caps_address = read_arg(engine, ArgReg::R9, "IDirect3D9::GetDeviceCaps")?;

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
        write_caps_u32(
            engine,
            caps_address,
            88,
            MAX_TEXTURE_DIMENSION,
            "MaxTextureWidth",
        )?;
        write_caps_u32(
            engine,
            caps_address,
            92,
            MAX_TEXTURE_DIMENSION,
            "MaxTextureHeight",
        )?;

        // TextureCaps — P4b: the sampler handles non-power-of-two sizes with
        // wrap/clamp address modes, so claim NONPOW2CONDITIONAL.
        // POW2 is trivially a subset and needs no separate claim.
        write_caps_u32(
            engine,
            caps_address,
            60,
            D3DPTEXTURECAPS_NONPOW2CONDITIONAL,
            "TextureCaps",
        )?;

        // MaxVolumeExtent
        write_caps_u32(engine, caps_address, 96, 256, "MaxVolumeExtent")?;

        // MaxTextureRepeat
        write_caps_u32(engine, caps_address, 100, 8192, "MaxTextureRepeat")?;

        // MaxTextureAspectRatio
        write_caps_u32(
            engine,
            caps_address,
            104,
            MAX_TEXTURE_DIMENSION,
            "MaxTextureAspectRatio",
        )?;

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

        // VertexShaderVersion — the vs_2_0 interpreter runs in the vertex
        // stage (FVF decode → v0..v15 → the VS → oPos → viewport transform),
        // so the caps honestly report D3DVS_VERSION(2,0) = 0xFFFE0200
        // (D3DVS_VERSION's tag is 0xFFFE0000 — the 0xFFFF tag is the pixel
        // shader's). Games branching on `VertexShaderVersion != 0` now take
        // the programmable path we actually implement.
        write_caps_u32(
            engine,
            caps_address,
            196,
            D3DVS_VERSION_2_0,
            "VertexShaderVersion",
        )?;

        // MaxVertexShaderConst — the vs_2_0 constant file is implemented
        // (256 float4s; the size the caps report).
        write_caps_u32(engine, caps_address, 200, 256, "MaxVertexShaderConst")?;

        // PixelShaderVersion — the ps_2_0 interpreter runs in the
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

    ctx.finish(return_value)
}

/// Handles `IDirect3D9::GetAdapterDisplayMode`.
pub fn handle_get_adapter_display_mode(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::GetAdapterDisplayMode")?;

    let adapter = read_arg(engine, ArgReg::Rdx, "IDirect3D9::GetAdapterDisplayMode")?;

    let display_mode_address = read_arg(engine, ArgReg::R8, "IDirect3D9::GetAdapterDisplayMode")?;

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

    ctx.finish(return_value)
}

pub(crate) fn allocate_direct3d_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
    _allocation_name: &str,
) -> u64 {
    state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, size)
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

    read_u64(engine, argument_address)
}

fn normalize_presentation_parameters(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    parameters_address: u64,
    focus_window: u64,
) -> Result<()> {
    let width = read_u32(engine, parameters_address).context("failed to read BackBufferWidth")?;

    let height_address = parameters_address
        .checked_add(4)
        .context("BackBufferHeight address overflow")?;

    let height = read_u32(engine, height_address).context("failed to read BackBufferHeight")?;

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
        read_u64(engine, device_window_address).context("failed to read hDeviceWindow")?;

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
    let width = read_u32(engine, parameters_address).context("failed to read BackBufferWidth")?;
    let height_address = parameters_address
        .checked_add(4)
        .context("BackBufferHeight address overflow")?;
    let height = read_u32(engine, height_address).context("failed to read BackBufferHeight")?;

    // On Win64, hDeviceWindow is at offset 32 (HWND is 64-bit aligned).
    let device_window_address = parameters_address
        .checked_add(32)
        .context("hDeviceWindow address overflow")?;
    let present_hwnd =
        read_u64(engine, device_window_address).context("failed to read hDeviceWindow")?;

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
    d3d.d3d9_scene_active = crate::state::SceneState::Inactive;
    d3d.d3d9_world_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_view_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_projection_matrix = crate::d3d9_render::IDENTITY;
    d3d.d3d9_viewport = (0, 0, width, height, 0.0, 1.0);
    d3d.d3d9_dirty = None;
    d3d.d3d9_stream_source_va = 0;
    d3d.d3d9_stream_stride = 0;
    d3d.d3d9_stream_offset = 0;
    d3d.d3d9_index_buffer_va = 0;

    let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
    d3d.d3d9_backbuffer = vec![0; needed];
    Ok(())
}

/// Handles `IDirect3D9::CreateDevice`.
pub fn handle_create_device(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3D9::CreateDevice")?;

    let adapter = read_arg(engine, ArgReg::Rdx, "IDirect3D9::CreateDevice")?;

    let device_type = read_arg(engine, ArgReg::R8, "IDirect3D9::CreateDevice")?;

    let focus_window = read_arg(engine, ArgReg::R9, "IDirect3D9::CreateDevice")?;

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

    ctx.finish(return_value)
}
