use anyhow::{Context, Result};

use crate::d3d9_render::{
    D3DPT_TRIANGLEFAN, D3DPT_TRIANGLELIST, D3DPT_TRIANGLESTRIP, D3DRS_ALPHABLENDENABLE,
    D3DRS_BLENDOP, D3DRS_DESTBLEND, D3DRS_SRCBLEND, D3DRS_ZENABLE, D3DRS_ZFUNC, D3DRS_ZWRITEENABLE,
    D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER, D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER,
    D3DTOP_DISABLE, D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP, D3DTSS_COLORARG1,
    D3DTSS_COLORARG2, D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX, D3dBlend, D3dBlendOp, D3dCmpFunc,
    D3dZBufferType, FragmentState, FvfLayout, GuestVertex, Mat4, PsProgram, RenderState,
    TextureStage, TextureStageState, Viewport, draw_triangle, mat4_mul, parse_fvf, parse_vertex,
};
use crate::d3d9_shader::{PS_SAMPLER_COUNT, ShaderKind, ShaderRecord, parse_shader};
use crate::fake_va::{
    D3d9Iface, Device9Method, Direct3D9Method, PixelShader9Method, Surface9Method, Texture9Method,
    encode_com,
};
use crate::guest_memory::{
    checked_field_address, read_u32 as read_guest_u32, read_u64 as read_guest_u64,
    write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

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

const D3D_OK: u64 = 0;
const D3DERR_INVALIDCALL: u64 = 0x8876_086c;

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

const D3DFMT_X8R8G8B8: u32 = 22;

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
const D3DFMT_INDEX32: u32 = 102;

/// `D3DFMT_A8R8G8B8` — 32-bpp with alpha (texels stored `0xAARRGGBB`).
const D3DFMT_A8R8G8B8: u32 = 21;

/// `D3DFMT_D24S8` — 32-bpp depth+stencil (75; stencil bits unused in P4c).
const D3DFMT_D24S8: u32 = 75;
/// `D3DFMT_D16` — 16-bpp depth (80).
const D3DFMT_D16: u32 = 80;

/// Number of methods in the `IDirect3DTexture9` vtable (0..21).
pub const IDIRECT3DTEXTURE9_METHOD_COUNT: usize = Texture9Method::VTABLE_SLOTS;
/// Space reserved for the texture vtable + COM object.
const IDIRECT3DTEXTURE9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 22-entry vtable.
const IDIRECT3DTEXTURE9_OBJECT_OFFSET: u64 = 0x80;

/// Number of methods in the `IDirect3DSurface9` vtable (0..17).
pub const IDIRECT3DSURFACE9_METHOD_COUNT: usize = Surface9Method::VTABLE_SLOTS;
/// Space reserved for the surface vtable + COM object.
const IDIRECT3DSURFACE9_ALLOCATION_SIZE: u64 = 0x100;
/// Offset of the COM object after the 18-entry vtable.
const IDIRECT3DSURFACE9_OBJECT_OFFSET: u64 = 0x80;

/// A D3D9 texture: host-owned texels plus the guest lock state.
///
/// Texels are stored in D3DCOLOR order (`0xAARRGGBB`, matching what the guest
/// writes through `LockRect`), row-major, top row first. The fragment stage
/// samples them and masks to 0RGB when writing the backbuffer.
#[derive(Debug, Clone)]
pub struct TextureRecord {
    /// The texture object's guest VA (also the `IDirect3DTexture9` pointer).
    pub handle: u64,
    /// Texture width in texels.
    pub width: u32,
    /// Texture height in texels.
    pub height: u32,
    /// Number of levels (slice: `levels == 0` → 1; mip chain deferred).
    pub levels: u32,
    /// `D3DFMT_*` format (only A8R8G8B8 / X8R8G8B8 are accepted).
    pub format: u32,
    /// Texels in `0xAARRGGBB` order.
    pub pixels: Vec<u32>,
    /// Surface object VA handed out by `GetSurfaceLevel` (created lazily).
    pub surface_va: u64,
    /// Guest block VA handed out by the active `LockRect` (0 = not locked).
    pub locked_va: u64,
    /// Locked region (None = whole surface) in surface coordinates.
    pub locked_rect: Option<(i32, i32, i32, i32)>,
}

/// A D3D9 depth-stencil surface: host-owned depth texels.
///
/// Depth values are `f32` in `0.0 = near` .. `1.0 = far` (D3D9's cleared
/// default). `D3DFMT_D24S8` stores depth only — the stencil bits are unused
/// (documented; stencil ops are out of P4c scope).
#[derive(Debug, Clone)]
pub struct DepthStencilRecord {
    /// The surface object's guest VA (also the `IDirect3DSurface9` pointer).
    pub handle: u64,
    /// Surface width in pixels.
    pub width: u32,
    /// Surface height in pixels.
    pub height: u32,
    /// `D3DFMT_*` format (only D16 / D24S8 are accepted).
    pub format: u32,
    /// Depth texels, row-major (0.0 = near, 1.0 = far initial value).
    pub depth: Vec<f32>,
}

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

fn allocate_direct3d_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
    _allocation_name: &str,
) -> u64 {
    state.heap_state.heap.alloc_coherent(engine, size)
}

fn read_stack_argument(
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

// ── P5a shader objects ─────────────────────────────────────────────────
//
// Shader objects are fake COM allocations (vtable + object) like textures;
// the guest holds the object pointer and calls the IUnknown trio through the
// fake vtable. The bytecode is copied host-side at Create* time (the guest
// buffer is transient) and tokenized into ParsedShader.

/// Number of methods in the `IDirect3DPixelShader9` / `IDirect3DVertexShader9`
/// vtables (the IUnknown trio only).
pub const IDIRECT3DSHADER9_METHOD_COUNT: usize = PixelShader9Method::VTABLE_SLOTS;
/// Space reserved for a shader vtable + COM object.
const IDIRECT3DSHADER9_ALLOCATION_SIZE: u64 = 0x40;
/// Offset of the COM object after the 3-entry vtable.
const IDIRECT3DSHADER9_OBJECT_OFFSET: u64 = 0x20;

/// Walk the guest bytecode pointer and copy it host-side.
///
/// Reads DWORD tokens one at a time (never holds a guest pointer); stops at
/// the `end` opcode. The instruction-length walk uses the same per-opcode
/// operand counts as the tokenizer, so malformed streams fail here with
/// `D3DERR_INVALIDCALL` rather than reading past the buffer.
fn read_shader_bytecode(
    engine: &mut dyn wie_cpu::CpuEngine,
    bytecode_ptr: u64,
) -> Result<Vec<u32>> {
    const MAX_TOKENS: usize = crate::d3d9_shader::MAX_SHADER_TOKENS;
    let mut tokens = Vec::new();
    // Version token.
    let version = read_guest_u32(engine, bytecode_ptr).context("failed to read shader version")?;
    tokens.push(version);
    let mut offset: u64 = 4;
    let _ = crate::d3d9_shader::decode_shader_version(version)
        .context("unsupported shader version token")?;
    loop {
        if tokens.len() >= MAX_TOKENS {
            anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
        }
        let address = bytecode_ptr.wrapping_add(offset);
        let token = read_guest_u32(engine, address)
            .context("failed to read shader token from guest memory")?;
        offset = offset.wrapping_add(4);
        tokens.push(token);
        let opcode = token & crate::d3d9_shader::OPCODE_FIELD_MASK;
        if opcode == crate::d3d9_shader::D3DSIO_END {
            break;
        }
        if opcode == crate::d3d9_shader::D3DSIO_COMMENT {
            let payload = usize::try_from(
                (token & crate::d3d9_shader::COMMENTSIZE_FIELD_MASK)
                    >> crate::d3d9_shader::COMMENTSIZE_FIELD_SHIFT,
            )
            .context("comment payload too large")?;
            for _ in 0..payload {
                if tokens.len() >= MAX_TOKENS {
                    anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
                }
                let comment_address = bytecode_ptr.wrapping_add(offset);
                let comment_token = read_guest_u32(engine, comment_address)
                    .context("failed to read shader comment payload")?;
                offset = offset.wrapping_add(4);
                tokens.push(comment_token);
            }
            continue;
        }
        let payload_len =
            crate::d3d9_shader::instruction_payload_len(opcode).context("unknown shader opcode")?;
        for _ in 0..payload_len {
            if tokens.len() >= MAX_TOKENS {
                anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
            }
            let operand_address = bytecode_ptr.wrapping_add(offset);
            let operand_token = read_guest_u32(engine, operand_address)
                .context("failed to read shader operand token")?;
            offset = offset.wrapping_add(4);
            tokens.push(operand_token);
        }
    }
    Ok(tokens)
}

/// Allocate a shader object (vtable + COM object) and return its pointer.
fn allocate_shader_object(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    iface: D3d9Iface,
) -> Result<u64> {
    let vtable_address = allocate_direct3d_block(
        engine,
        state,
        IDIRECT3DSHADER9_ALLOCATION_SIZE,
        "IDirect3DShader9",
    );
    if vtable_address == 0 {
        return Ok(0);
    }
    fill_com_vtable(engine, vtable_address, iface, IDIRECT3DSHADER9_METHOD_COUNT)?;
    let object_address = vtable_address
        .checked_add(IDIRECT3DSHADER9_OBJECT_OFFSET)
        .context("IDirect3DShader9 object address overflow")?;
    write_guest_u64(engine, object_address, vtable_address)
        .context("failed to initialize IDirect3DShader9 object")?;
    Ok(object_address)
}

/// Handles `IDirect3DDevice9::CreatePixelShader` (vtable slot 106).
///
/// Copies the guest bytecode (ps_2_0 family only), tokenizes it, and rejects
/// malformed bytecode or shaders whose opcodes the P5a-1 interpreter cannot
/// execute (`D3DERR_INVALIDCALL` — the full instruction set is P5a-2).
pub fn handle_create_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreatePixelShader")?;
    let bytecode_ptr = engine
        .read_rdx()
        .context("failed to read RDX for CreatePixelShader")?;
    let pp_shader = engine
        .read_r8()
        .context("failed to read R8 for CreatePixelShader")?;

    let return_value = if bytecode_ptr != 0 && pp_shader != 0 {
        let bytecode = read_shader_bytecode(engine, bytecode_ptr);
        let parsed = bytecode
            .as_deref()
            .ok()
            .and_then(|tokens| parse_shader(tokens).ok());
        match (bytecode, parsed) {
            (Ok(bytecode), Some(parsed))
                if parsed.kind == ShaderKind::Pixel && parsed.is_fully_executable() =>
            {
                let object = allocate_shader_object(engine, state, D3d9Iface::PixelShader9)?;
                if object == 0 {
                    D3DERR_INVALIDCALL
                } else {
                    state.d3d9().d3d9_shaders.insert(
                        object,
                        ShaderRecord {
                            handle: object,
                            kind: ShaderKind::Pixel,
                            bytecode,
                            parsed,
                        },
                    );
                    // `def` writes the device constant registers (real D3D9
                    // semantics); later SetPixelShaderConstantF calls override.
                    let d3d = state.d3d9();
                    let def_constants = d3d
                        .d3d9_shaders
                        .get(&object)
                        .map(|record| record.parsed.constants.clone())
                        .unwrap_or_default();
                    for (register, value) in &def_constants {
                        if let Some(slot) = d3d
                            .d3d9_ps_constants
                            .get_mut(usize::try_from(*register).unwrap_or(usize::MAX))
                        {
                            *slot = *value;
                        }
                    }
                    write_guest_u64(engine, pp_shader, object)
                        .context("failed to return IDirect3DPixelShader9 pointer")?;
                    D3D_OK
                }
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreatePixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::CreateVertexShader` (vtable slot 91).
///
/// Accepts vs_2_0 bytecode and stores the object (it must round-trip and
/// bind); the vertex stage keeps the FFP Gouraud path until P5a-2 executes
/// vertex shaders.
pub fn handle_create_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreateVertexShader")?;
    let bytecode_ptr = engine
        .read_rdx()
        .context("failed to read RDX for CreateVertexShader")?;
    let pp_shader = engine
        .read_r8()
        .context("failed to read R8 for CreateVertexShader")?;

    let return_value = if bytecode_ptr != 0 && pp_shader != 0 {
        let bytecode = read_shader_bytecode(engine, bytecode_ptr);
        let parsed = bytecode
            .as_deref()
            .ok()
            .and_then(|tokens| parse_shader(tokens).ok());
        match (bytecode, parsed) {
            (Ok(bytecode), Some(parsed)) if parsed.kind == ShaderKind::Vertex => {
                let object = allocate_shader_object(engine, state, D3d9Iface::VertexShader9)?;
                if object == 0 {
                    D3DERR_INVALIDCALL
                } else {
                    state.d3d9().d3d9_shaders.insert(
                        object,
                        ShaderRecord {
                            handle: object,
                            kind: ShaderKind::Vertex,
                            bytecode,
                            parsed,
                        },
                    );
                    write_guest_u64(engine, pp_shader, object)
                        .context("failed to return IDirect3DVertexShader9 pointer")?;
                    D3D_OK
                }
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Common Set*Shader body: validate the pointer (NULL clears) and bind it.
fn set_shader_binding(state: &mut WinApiState, shader: u64, kind: ShaderKind) {
    let d3d = state.d3d9();
    let known = shader == 0
        || d3d
            .d3d9_shaders
            .get(&shader)
            .is_some_and(|record| record.kind == kind);
    if !known {
        return;
    }
    let d3d = state.d3d9();
    match kind {
        ShaderKind::Pixel => d3d.d3d9_pixel_shader = shader,
        ShaderKind::Vertex => d3d.d3d9_current_vertex_shader = shader,
    }
}

/// Handles `IDirect3DDevice9::SetPixelShader` (vtable slot 107).
pub fn handle_set_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetPixelShader")?;
    let shader = engine
        .read_rdx()
        .context("failed to read RDX for SetPixelShader")?;

    set_shader_binding(state, shader, ShaderKind::Pixel);

    let return_value = D3D_OK;
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetPixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::GetPixelShader` (vtable slot 108).
pub fn handle_get_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetPixelShader")?;
    let pp_shader = engine
        .read_rdx()
        .context("failed to read RDX for GetPixelShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_pixel_shader)
            .context("failed to write GetPixelShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetPixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetVertexShader` (vtable slot 92).
pub fn handle_set_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShader")?;
    let vertex_shader = engine
        .read_rdx()
        .context("failed to read RDX for SetVertexShader")?;

    set_shader_binding(state, vertex_shader, ShaderKind::Vertex);

    let return_value = D3D_OK;
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShader` (vtable slot 93).
pub fn handle_get_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShader")?;
    let pp_shader = engine
        .read_rdx()
        .context("failed to read RDX for GetVertexShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_current_vertex_shader)
            .context("failed to write GetVertexShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Common Set*ShaderConstantF body: copy `count` float4s from the guest.
///
/// Reads the raw LE `f32` bytes with the guest memory helpers; registers past
/// the file's end are dropped (the writes are clamped to the file).
fn set_shader_constant_f(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [[f32; 4]],
    start_register: u32,
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let float_address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        engine
            .mem_read(float_address, &mut bytes)
            .context("failed to read shader constant data")?;
        let mut chunks = [[0_u8; 4]; 4];
        for (chunk, byte_chunk) in chunks.iter_mut().zip(bytes.chunks_exact(4)) {
            chunk.copy_from_slice(byte_chunk);
        }
        for (channel, chunk) in chunks.into_iter().enumerate() {
            if let Some(slot_channel) = slot.get_mut(channel) {
                *slot_channel = f32::from_le_bytes(chunk);
            }
        }
    }
    Ok(())
}

/// Common Get*ShaderConstantF body: copy `count` float4s to the guest.
fn get_shader_constant_f(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &[[f32; 4]],
    start_register: u32,
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        for (channel, byte_chunk) in bytes.chunks_exact_mut(4).enumerate() {
            let chunk = value.get(channel).copied().unwrap_or(0.0).to_le_bytes();
            byte_chunk.copy_from_slice(&chunk);
        }
        engine
            .mem_write(address, &bytes)
            .context("failed to write shader constant data")?;
    }
    Ok(())
}

/// Handles `IDirect3DDevice9::SetPixelShaderConstantF` (vtable slot 109).
pub fn handle_set_pixel_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetPixelShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetPixelShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_ps_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetPixelShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetPixelShaderConstantF` (vtable slot 110).
pub fn handle_get_pixel_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetPixelShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetPixelShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_ps_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetPixelShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::SetVertexShaderConstantF` (vtable slot 94).
///
/// Stores into the vs_2_0 constant file (256 float4s); the values are used
/// when P5a-2 executes vertex shaders.
pub fn handle_set_vertex_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetVertexShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_vs_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetVertexShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantF` (vtable slot 95).
pub fn handle_get_vertex_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetVertexShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_vs_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetVertexShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DPixelShader9::Release` (vtable slot 2).
pub fn handle_pixel_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DPixelShader9::Release")?;

    let return_value = if state.d3d9().d3d9_shaders.remove(&this_pointer).is_some() {
        if state.d3d9().d3d9_pixel_shader == this_pointer {
            state.d3d9().d3d9_pixel_shader = 0;
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DSHADER9_OBJECT_OFFSET);
        let _ = state.heap_state.heap.free_coherent(engine, vtable);
        1
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DPixelShader9::Release")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DVertexShader9::Release` (vtable slot 2).
pub fn handle_vertex_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DVertexShader9::Release")?;

    let return_value = if state.d3d9().d3d9_shaders.remove(&this_pointer).is_some() {
        if state.d3d9().d3d9_current_vertex_shader == this_pointer {
            state.d3d9().d3d9_current_vertex_shader = 0;
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DSHADER9_OBJECT_OFFSET);
        let _ = state.heap_state.heap.free_coherent(engine, vtable);
        1
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DVertexShader9::Release")?;
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

// ── P3 software-render handlers (slice 1) ────────────────────────────────
//
// The guest renders through the D3D9 pipeline: Clear fills the host-owned
// backbuffer, Draw*(UP) rasterize triangles into it (host CPU), and Present
// publishes it through the existing PresentState surface pipeline — the same
// path GDI BitBlt uses.

/// Read one little-endian `f32` from a fixed-size byte buffer at `offset`.
fn read_f32_at(bytes: &[u8], offset: usize) -> f32 {
    let end = offset.saturating_add(4);
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    f32::from_le_bytes(raw)
}

/// Read one little-endian `u32` from a fixed-size byte buffer at `offset`.
fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    let end = offset.saturating_add(4);
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    u32::from_le_bytes(raw)
}

/// Parse a column-major `D3DMATRIX` (16 floats) from a byte buffer.
fn parse_mat4(bytes: &[u8; 64]) -> Mat4 {
    let mut matrix = [0.0_f32; 16];
    for (index, slot) in matrix.iter_mut().enumerate() {
        *slot = read_f32_at(bytes, index.saturating_mul(4));
    }
    matrix
}

/// Fill one clipped rect of the backbuffer with `color` (0RGB).
#[allow(clippy::too_many_arguments)]
fn fill_backbuffer_rect(
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
fn primitive_vertex_count(primitive_type: u64, primitive_count: u64) -> Option<usize> {
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
            for i in 0..count {
                let base = i.saturating_mul(3);
                triples.push((base, base.saturating_add(1), base.saturating_add(2)));
            }
        }
        D3DPT_TRIANGLESTRIP => {
            for i in 0..count {
                triples.push((i, i.saturating_add(1), i.saturating_add(2)));
            }
        }
        D3DPT_TRIANGLEFAN => {
            for i in 0..count {
                triples.push((0, i.saturating_add(1), i.saturating_add(2)));
            }
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
    depth_surfaces: &'a mut std::collections::HashMap<u64, DepthStencilRecord>,
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
    textures: &'a std::collections::HashMap<u64, TextureRecord>,
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
    textures: &'a std::collections::HashMap<u64, TextureRecord>,
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
    textures: &'a std::collections::HashMap<u64, TextureRecord>,
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
/// rejects triangles behind the near plane, and accumulates the dirty region.
/// When a stage-0 texture is bound and enabled, the fragment stage samples it.
#[allow(clippy::too_many_arguments)]
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
#[allow(clippy::too_many_arguments)]
fn draw_vertex_stream(
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

// ── P4b texture handlers ────────────────────────────────────────────────
//
// Textures are host-owned texel buffers with guest-visible lock blocks: the
// guest LockRects a heap block (never a host pointer — the soft-translate
// rule), fills it through normal guest memory writes, and UnlockRect copies
// the region back into the host TextureRecord.

/// Fill the vtable slots of a freshly allocated texture/surface object.
fn fill_com_vtable(
    engine: &mut dyn wie_cpu::CpuEngine,
    vtable_address: u64,
    iface: D3d9Iface,
    method_count: usize,
) -> Result<()> {
    for slot in 0..method_count {
        let slot_u64 = u64::try_from(slot).context("vtable slot does not fit u64")?;
        let byte_offset = slot_u64.checked_mul(8).context("vtable offset overflow")?;
        let entry_address = vtable_address
            .checked_add(byte_offset)
            .context("vtable entry address overflow")?;
        let method = u8::try_from(slot).context("vtable slot does not fit u8")?;
        write_guest_u64(engine, entry_address, encode_com(iface, method))?;
    }
    Ok(())
}

/// Allocate a texture object (vtable + COM object) and return its pointer.
fn allocate_texture_object(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<u64> {
    let vtable_address = allocate_direct3d_block(
        engine,
        state,
        IDIRECT3DTEXTURE9_ALLOCATION_SIZE,
        "IDirect3DTexture9",
    );
    if vtable_address == 0 {
        return Ok(0);
    }
    fill_com_vtable(
        engine,
        vtable_address,
        D3d9Iface::Texture9,
        IDIRECT3DTEXTURE9_METHOD_COUNT,
    )?;
    let object_address = vtable_address
        .checked_add(IDIRECT3DTEXTURE9_OBJECT_OFFSET)
        .context("IDirect3DTexture9 object address overflow")?;
    write_guest_u64(engine, object_address, vtable_address)
        .context("failed to initialize IDirect3DTexture9 object")?;
    Ok(object_address)
}

/// Allocate a surface object (vtable + COM object) and return its pointer.
fn allocate_surface_object(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<u64> {
    let vtable_address = allocate_direct3d_block(
        engine,
        state,
        IDIRECT3DSURFACE9_ALLOCATION_SIZE,
        "IDirect3DSurface9",
    );
    if vtable_address == 0 {
        return Ok(0);
    }
    fill_com_vtable(
        engine,
        vtable_address,
        D3d9Iface::Surface9,
        IDIRECT3DSURFACE9_METHOD_COUNT,
    )?;
    let object_address = vtable_address
        .checked_add(IDIRECT3DSURFACE9_OBJECT_OFFSET)
        .context("IDirect3DSurface9 object address overflow")?;
    write_guest_u64(engine, object_address, vtable_address)
        .context("failed to initialize IDirect3DSurface9 object")?;
    Ok(object_address)
}

/// Handles `IDirect3DDevice9::CreateTexture` (vtable slot 23).
///
/// Formats: `D3DFMT_A8R8G8B8` (21) and `D3DFMT_X8R8G8B8` (22). `levels == 0`
/// becomes 1 level — the full mip chain is deferred. The texture is host-owned;
/// the guest fills it through `GetSurfaceLevel` → `LockRect`/`UnlockRect`.
pub fn handle_create_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreateTexture")?;
    let width_raw = engine
        .read_rdx()
        .context("failed to read RDX for CreateTexture")?;
    let height_raw = engine
        .read_r8()
        .context("failed to read R8 for CreateTexture")?;
    let levels_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateTexture")?;
    let _usage = read_stack_argument(engine, 0x28, "CreateTexture Usage")?;
    let format_raw = read_stack_argument(engine, 0x30, "CreateTexture Format")?;
    let _pool = read_stack_argument(engine, 0x38, "CreateTexture Pool")?;
    let pp_texture = read_stack_argument(engine, 0x40, "CreateTexture ppTexture")?;
    let _shared_handle = read_stack_argument(engine, 0x48, "CreateTexture pSharedHandle")?;

    let width = u32::try_from(width_raw & u64::from(u32::MAX))
        .context("CreateTexture width does not fit u32")?;
    let height = u32::try_from(height_raw & u64::from(u32::MAX))
        .context("CreateTexture height does not fit u32")?;
    let levels = u32::try_from(levels_raw & u64::from(u32::MAX))
        .context("CreateTexture levels does not fit u32")?
        .max(1);
    let format = u32::try_from(format_raw & u64::from(u32::MAX))
        .context("CreateTexture format does not fit u32")?;

    let valid = width > 0
        && height > 0
        && width <= 4096
        && height <= 4096
        && matches!(format, D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8)
        && pp_texture != 0;

    let return_value = if valid {
        let object = allocate_texture_object(engine, state)?;
        if object == 0 {
            D3DERR_INVALIDCALL
        } else {
            let texel_count = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
            state.d3d9().d3d9_textures.insert(
                object,
                TextureRecord {
                    handle: object,
                    width,
                    height,
                    levels,
                    format,
                    pixels: vec![0; texel_count],
                    surface_va: 0,
                    locked_va: 0,
                    locked_rect: None,
                },
            );
            write_guest_u64(engine, pp_texture, object)
                .context("failed to return IDirect3DTexture9 pointer")?;
            D3D_OK
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateTexture")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DTexture9::GetSurfaceLevel` (vtable slot 18).
///
/// Slice model: the texture record IS the surface; level 0 is the whole
/// texture (higher levels return the same record — mips deferred). The
/// surface object is created lazily and cached on the record.
pub fn handle_texture_get_surface_level(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetSurfaceLevel")?;
    let _level = engine
        .read_rdx()
        .context("failed to read RDX for GetSurfaceLevel")?;
    let pp_surface = engine
        .read_r8()
        .context("failed to read R8 for GetSurfaceLevel")?;

    let valid_texture = state.d3d9().d3d9_textures.contains_key(&this_pointer);
    let return_value = if valid_texture && pp_surface != 0 {
        let surface = match state.d3d9().d3d9_textures.get(&this_pointer) {
            Some(record) if record.surface_va != 0 => record.surface_va,
            _ => {
                let object = allocate_surface_object(engine, state)?;
                if object == 0 {
                    0
                } else {
                    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&this_pointer) {
                        record.surface_va = object;
                    }
                    state
                        .d3d9()
                        .d3d9_surface_textures
                        .insert(object, this_pointer);
                    object
                }
            }
        };
        if surface == 0 {
            D3DERR_INVALIDCALL
        } else {
            write_guest_u64(engine, pp_surface, surface)
                .context("failed to return IDirect3DSurface9 pointer")?;
            D3D_OK
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSurfaceLevel")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Read a guest RECT (four i32s) at `rect_ptr`.
fn read_guest_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    rect_ptr: u64,
) -> Result<Option<(i32, i32, i32, i32)>> {
    if rect_ptr == 0 {
        return Ok(None);
    }
    let mut bytes = [0_u8; 16];
    engine
        .mem_read(rect_ptr, &mut bytes)
        .context("failed to read lock RECT")?;
    let read_i32_at = |off: usize| -> i32 {
        i32::from_le_bytes(
            bytes
                .get(off..off.saturating_add(4))
                .and_then(|s| s.try_into().ok())
                .unwrap_or([0; 4]),
        )
    };
    Ok(Some((
        read_i32_at(0),
        read_i32_at(4),
        read_i32_at(8),
        read_i32_at(12),
    )))
}

/// Shared LockRect body: allocate a guest block, point `pLockedRect` at it
/// (or at the rect's top-left), and remember the region for the copy-back.
fn lock_rect_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    texture_va: u64,
    p_locked_rect: u64,
    p_rect: u64,
) -> Result<u64> {
    let Some(record) = state.d3d9().d3d9_textures.get(&texture_va) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    let (width, height) = (record.width, record.height);
    if record.locked_va != 0 || width == 0 || height == 0 {
        return Ok(D3DERR_INVALIDCALL); // double lock / degenerate
    }
    let pitch = width.checked_mul(4).context("texture pitch overflow")?;
    let total = u64::from(pitch)
        .checked_mul(u64::from(height))
        .context("texture lock size overflow")?;

    let rect = read_guest_rect(engine, p_rect)?;
    if let Some((left, top, right, bottom)) = rect {
        let within = left >= 0
            && top >= 0
            && right > left
            && bottom > top
            && right <= i32::try_from(width).unwrap_or(0)
            && bottom <= i32::try_from(height).unwrap_or(0);
        if !within {
            return Ok(D3DERR_INVALIDCALL);
        }
    }

    let block = state.heap_state.heap.alloc_coherent(engine, total);
    if block == 0 {
        return Ok(D3DERR_INVALIDCALL); // allocation failed
    }
    let (left, top) = rect.map_or((0_i32, 0_i32), |r| (r.0, r.1));
    let offset = u64::try_from(i64::from(top).saturating_mul(i64::from(pitch)))
        .unwrap_or(0)
        .saturating_add(u64::try_from(i64::from(left).saturating_mul(4)).unwrap_or(0));
    let p_bits = block.saturating_add(offset);

    if p_locked_rect != 0 {
        write_guest_u32(engine, p_locked_rect, pitch)
            .context("failed to write D3DLOCKED_RECT.Pitch")?;
        write_guest_u64(
            engine,
            checked_field_address(p_locked_rect, 8, "D3DLOCKED_RECT.pBits"),
            p_bits,
        )
        .context("failed to write D3DLOCKED_RECT.pBits")?;
    }

    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
        record.locked_va = block;
        record.locked_rect = rect;
    }
    Ok(D3D_OK)
}

/// Handles `IDirect3DSurface9::LockRect` (vtable slot 13).
pub fn handle_surface_lock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DSurface9::LockRect")?;
    let p_locked_rect = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DSurface9::LockRect")?;
    let p_rect = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DSurface9::LockRect")?;
    let _flags = read_stack_argument(engine, 0x28, "LockRect Flags")?;

    let texture_va = state
        .d3d9()
        .d3d9_surface_textures
        .get(&this_pointer)
        .copied()
        .unwrap_or(0);
    let return_value = if texture_va != 0 {
        lock_rect_common(engine, state, texture_va, p_locked_rect, p_rect)?
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DSurface9::LockRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DTexture9::LockRect` (vtable slot 19) — the deprecated
/// texture-level form; `RDX` is the level (deferred, must be 0).
pub fn handle_texture_lock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DTexture9::LockRect")?;
    let level_raw = engine
        .read_rdx()
        .context("failed to read RDX for IDirect3DTexture9::LockRect")?;
    let p_locked_rect = engine
        .read_r8()
        .context("failed to read R8 for IDirect3DTexture9::LockRect")?;
    let p_rect = engine
        .read_r9()
        .context("failed to read R9 for IDirect3DTexture9::LockRect")?;
    let _flags = read_stack_argument(engine, 0x28, "IDirect3DTexture9::LockRect Flags")?;

    let level = level_raw & u64::from(u32::MAX);
    let return_value = if level == 0 {
        lock_rect_common(engine, state, this_pointer, p_locked_rect, p_rect)?
    } else {
        // Mips are deferred — only level 0 exists.
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DTexture9::LockRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Shared UnlockRect body: copy the locked region back into the host texels
/// and free the guest block.
fn unlock_rect_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    texture_va: u64,
) -> u64 {
    let Some(record) = state.d3d9().d3d9_textures.get(&texture_va) else {
        return D3DERR_INVALIDCALL;
    };
    let locked_va = record.locked_va;
    if locked_va == 0 {
        return D3DERR_INVALIDCALL; // not locked
    }
    let (width, height) = (record.width, record.height);
    let rect = record.locked_rect;
    let pitch = u64::from(width.checked_mul(4).unwrap_or(0));
    let (left, top, right, bottom) = rect.unwrap_or((
        0,
        0,
        i32::try_from(width).unwrap_or(0),
        i32::try_from(height).unwrap_or(0),
    ));
    let (left, top) = (left.max(0), top.max(0));
    let (right, bottom) = (
        right.min(i32::try_from(width).unwrap_or(0)),
        bottom.min(i32::try_from(height).unwrap_or(0)),
    );
    if left < right && top < bottom {
        let row_width = usize::try_from(right.saturating_sub(left)).unwrap_or(0);
        let mut row_bytes = vec![0_u8; row_width.saturating_mul(4)];
        for row in top..bottom {
            let src = locked_va
                .saturating_add(
                    u64::try_from(i64::from(row).saturating_mul(i64::try_from(pitch).unwrap_or(0)))
                        .unwrap_or(0),
                )
                .saturating_add(u64::try_from(i64::from(left).saturating_mul(4)).unwrap_or(0));
            if engine.mem_read(src, &mut row_bytes).is_err() {
                continue;
            }
            // Copy the row into the host texels (D3DCOLOR byte order).
            let mut texels: Vec<u32> = Vec::with_capacity(row_width);
            for chunk in row_bytes.chunks_exact(4) {
                let bytes: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
                texels.push(u32::from_le_bytes(bytes));
            }
            if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
                let row_start = usize::try_from(row)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(width).unwrap_or(0));
                for (col, texel) in texels.into_iter().enumerate() {
                    let index = row_start
                        .saturating_add(usize::try_from(left).unwrap_or(0))
                        .saturating_add(col);
                    if let Some(slot) = record.pixels.get_mut(index) {
                        *slot = texel;
                    }
                }
            }
        }
    }
    let _ = state.heap_state.heap.free_coherent(engine, locked_va);
    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
        record.locked_va = 0;
        record.locked_rect = None;
    }
    D3D_OK
}

/// Handles `IDirect3DSurface9::UnlockRect` (vtable slot 14).
pub fn handle_surface_unlock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DSurface9::UnlockRect")?;

    let texture_va = state
        .d3d9()
        .d3d9_surface_textures
        .get(&this_pointer)
        .copied()
        .unwrap_or(0);
    let return_value = if texture_va != 0 {
        unlock_rect_common(engine, state, texture_va)
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DSurface9::UnlockRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DTexture9::UnlockRect` (vtable slot 20).
pub fn handle_texture_unlock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DTexture9::UnlockRect")?;

    let return_value = unlock_rect_common(engine, state, this_pointer);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DTexture9::UnlockRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetTexture` (vtable slot 65).
pub fn handle_set_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetTexture")?;
    let stage_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetTexture")?;
    let texture_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetTexture")?;

    let stage = u32::try_from(stage_raw & u64::from(u32::MAX))
        .context("SetTexture stage does not fit u32")?;
    if let Some(slot) = state
        .d3d9()
        .d3d9_texture_bindings
        .get_mut(usize::try_from(stage).unwrap_or(usize::MAX))
    {
        *slot = texture_ptr;
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetTexture")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetTexture` (vtable slot 64).
pub fn handle_get_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetTexture")?;
    let stage_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetTexture")?;
    let pp_texture = engine
        .read_r8()
        .context("failed to read R8 for GetTexture")?;

    let stage = u32::try_from(stage_raw & u64::from(u32::MAX))
        .context("GetTexture stage does not fit u32")?;
    let binding = state
        .d3d9()
        .d3d9_texture_bindings
        .get(usize::try_from(stage).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(0);
    if pp_texture != 0 {
        write_guest_u64(engine, pp_texture, binding)
            .context("failed to write GetTexture output")?;
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetTexture")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetTextureStageState` (vtable slot 66).
pub fn handle_get_texture_stage_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetTextureStageState")?;
    let stage_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetTextureStageState")?;
    let state_type_raw = engine
        .read_r8()
        .context("failed to read R8 for GetTextureStageState")?;
    let p_value = engine
        .read_r9()
        .context("failed to read R9 for GetTextureStageState")?;

    let stage = u32::try_from(stage_raw & u64::from(u32::MAX))
        .context("GetTextureStageState stage does not fit u32")?;
    let state_type = u32::try_from(state_type_raw & u64::from(u32::MAX))
        .context("GetTextureStageState state type does not fit u32")?;
    if p_value != 0 {
        // Modeled slots read their D3D9 default when unset; unmodeled slots
        // read the stored verbatim value (or 0).
        let value = match state
            .d3d9()
            .d3d9_stage_states
            .get(usize::try_from(stage).unwrap_or(usize::MAX))
        {
            Some(stage_state) => match state_type {
                D3DTSS_COLOROP => stage_state.color_op,
                D3DTSS_COLORARG1 => stage_state.color_arg1,
                D3DTSS_COLORARG2 => stage_state.color_arg2,
                D3DTSS_ALPHAOP => stage_state.alpha_op,
                D3DTSS_ALPHAARG1 => stage_state.alpha_arg1,
                D3DTSS_ALPHAARG2 => stage_state.alpha_arg2,
                D3DTSS_TEXCOORDINDEX => stage_state.tex_coord_index,
                _ => stage_state
                    .other_tss
                    .iter()
                    .find(|(slot, _)| *slot == state_type)
                    .map_or(0, |(_, v)| *v),
            },
            None => 0,
        };
        write_guest_u32(engine, p_value, value)
            .context("failed to write GetTextureStageState output")?;
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetTextureStageState")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetSamplerState` (vtable slot 68).
pub fn handle_get_sampler_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetSamplerState")?;
    let sampler_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetSamplerState")?;
    let state_type_raw = engine
        .read_r8()
        .context("failed to read R8 for GetSamplerState")?;
    let p_value = engine
        .read_r9()
        .context("failed to read R9 for GetSamplerState")?;

    let sampler = u32::try_from(sampler_raw & u64::from(u32::MAX))
        .context("GetSamplerState sampler does not fit u32")?;
    let state_type = u32::try_from(state_type_raw & u64::from(u32::MAX))
        .context("GetSamplerState state type does not fit u32")?;
    if p_value != 0 {
        let value = match state
            .d3d9()
            .d3d9_stage_states
            .get(usize::try_from(sampler).unwrap_or(usize::MAX))
        {
            Some(stage_state) => match state_type {
                D3DSAMP_ADDRESSU => stage_state.address_u,
                D3DSAMP_ADDRESSV => stage_state.address_v,
                D3DSAMP_MAGFILTER => stage_state.mag_filter,
                D3DSAMP_MINFILTER => stage_state.min_filter,
                D3DSAMP_MIPFILTER => stage_state.mip_filter,
                _ => stage_state
                    .other_sampler
                    .iter()
                    .find(|(slot, _)| *slot == state_type)
                    .map_or(0, |(_, v)| *v),
            },
            None => 0,
        };
        write_guest_u32(engine, p_value, value)
            .context("failed to write GetSamplerState output")?;
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetSamplerState")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DTexture9::Release` (vtable slot 2).
pub fn handle_texture_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DTexture9::Release")?;

    let (surface_va, locked_va) = state
        .d3d9()
        .d3d9_textures
        .get(&this_pointer)
        .map_or((0, 0), |r| (r.surface_va, r.locked_va));
    let exists =
        surface_va != 0 || locked_va != 0 || state.d3d9().d3d9_textures.contains_key(&this_pointer);

    let return_value = if exists {
        // Unbind from every stage.
        for slot in &mut state.d3d9().d3d9_texture_bindings {
            if *slot == this_pointer {
                *slot = 0;
            }
        }
        if surface_va != 0 {
            state.d3d9().d3d9_surface_textures.remove(&surface_va);
            let vtable = surface_va.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state.heap_state.heap.free_coherent(engine, vtable);
        }
        if locked_va != 0 {
            let _ = state.heap_state.heap.free_coherent(engine, locked_va);
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DTEXTURE9_OBJECT_OFFSET);
        let _ = state.heap_state.heap.free_coherent(engine, vtable);
        state.d3d9().d3d9_textures.remove(&this_pointer);
        1
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DTexture9::Release")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DSurface9::Release` (vtable slot 2).
///
/// Releases the surface view only — the texture record (and its texels)
/// survive until the texture itself is released.
pub fn handle_surface_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DSurface9::Release")?;

    let return_value =
        if let Some(texture_va) = state.d3d9().d3d9_surface_textures.remove(&this_pointer) {
            if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
                record.surface_va = 0;
            }
            let vtable = this_pointer.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state.heap_state.heap.free_coherent(engine, vtable);
            1
        } else if state
            .d3d9()
            .d3d9_depth_surfaces
            .remove(&this_pointer)
            .is_some()
        {
            // Depth-stencil surface: drop the record (and unbind if bound).
            if state.d3d9().d3d9_depth_stencil == this_pointer {
                state.d3d9().d3d9_depth_stencil = 0;
            }
            let vtable = this_pointer.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state.heap_state.heap.free_coherent(engine, vtable);
            1
        } else {
            0
        };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DSurface9::Release")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DTexture9::GetLevelCount` (vtable slot 13).
pub fn handle_texture_get_level_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetLevelCount")?;

    let levels = state
        .d3d9()
        .d3d9_textures
        .get(&this_pointer)
        .map_or(0, |r| r.levels);
    let return_value = u64::from(levels);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetLevelCount")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

// ── P4c blend + depth handlers ──────────────────────────────────────────

/// Handles `IDirect3DDevice9::GetRenderState` (vtable slot 58).
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
        // Modeled states read back from the typed struct; unmodeled ones read
        // 0 (D3D9's default for unused states).
        let rs = &state.d3d9().d3d9_render_state;
        let value = match state_id {
            D3DRS_ALPHABLENDENABLE => u32::from(rs.alpha_blend_enable),
            D3DRS_ZWRITEENABLE => u32::from(rs.z_write_enable),
            D3DRS_ZENABLE => rs.z_enable.as_u32(),
            D3DRS_ZFUNC => rs.z_func.as_u32(),
            D3DRS_SRCBLEND => rs.src_blend.as_u32(),
            D3DRS_DESTBLEND => rs.dest_blend.as_u32(),
            D3DRS_BLENDOP => rs.blend_op.as_u32(),
            _ => 0,
        };
        write_guest_u32(engine, p_value, value).context("failed to write GetRenderState output")?;
    }

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetRenderState")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateDepthStencilSurface")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetDepthStencilSurface")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDepthStencilSurface")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
