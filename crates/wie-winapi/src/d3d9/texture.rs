use anyhow::{Context, Result};

use super::{
    D3D_OK, D3DERR_INVALIDCALL, D3DFMT_A8R8G8B8, D3DFMT_X8R8G8B8,
    IDIRECT3DSURFACE9_ALLOCATION_SIZE, IDIRECT3DSURFACE9_METHOD_COUNT,
    IDIRECT3DSURFACE9_OBJECT_OFFSET, IDIRECT3DTEXTURE9_ALLOCATION_SIZE,
    IDIRECT3DTEXTURE9_METHOD_COUNT, IDIRECT3DTEXTURE9_OBJECT_OFFSET, allocate_direct3d_block,
    read_stack_argument,
};
use crate::d3d9_render::{
    D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER, D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER,
    D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP, D3DTSS_COLORARG1, D3DTSS_COLORARG2,
    D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX,
};
use crate::fake_va::{D3d9Iface, encode_com};
use crate::guest_memory::{
    checked_field_address, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

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

// ── P4b texture handlers ────────────────────────────────────────────────
//
// Textures are host-owned texel buffers with guest-visible lock blocks: the
// guest LockRects a heap block (never a host pointer — the soft-translate
// rule), fills it through normal guest memory writes, and UnlockRect copies
// the region back into the host TextureRecord.

/// Fill the vtable slots of a freshly allocated texture/surface object.
pub(crate) fn fill_com_vtable(
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
pub(crate) fn allocate_surface_object(
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
