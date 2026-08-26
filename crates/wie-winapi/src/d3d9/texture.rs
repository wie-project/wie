use crate::gdi32::{ArgReg, read_arg};
use anyhow::{Context, Result};

use super::{
    D3D_OK, D3DERR_INVALIDCALL, D3DFMT_A8R8G8B8, D3DFMT_X8R8G8B8,
    IDIRECT3DSURFACE9_ALLOCATION_SIZE, IDIRECT3DSURFACE9_METHOD_COUNT,
    IDIRECT3DSURFACE9_OBJECT_OFFSET, IDIRECT3DTEXTURE9_ALLOCATION_SIZE,
    IDIRECT3DTEXTURE9_METHOD_COUNT, IDIRECT3DTEXTURE9_OBJECT_OFFSET, MAX_TEXTURE_DIMENSION,
    allocate_direct3d_block, read_stack_argument,
};
use crate::d3d9_render::{
    D3DSAMP_ADDRESSU, D3DSAMP_ADDRESSV, D3DSAMP_MAGFILTER, D3DSAMP_MINFILTER, D3DSAMP_MIPFILTER,
    D3DTSS_ALPHAARG1, D3DTSS_ALPHAARG2, D3DTSS_ALPHAOP, D3DTSS_COLORARG1, D3DTSS_COLORARG2,
    D3DTSS_COLOROP, D3DTSS_TEXCOORDINDEX,
};
use crate::fake_va::{D3d9Iface, encode_com};
use crate::guest_memory::{
    checked_address, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
use crate::kernel32::low_u32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// `D3DRTYPE_SURFACE` — the `D3DRESOURCETYPE` a surface reports in GetDesc.
const D3DRTYPE_SURFACE: u32 = 3;

/// One mip level's host texels (levels 1..; level 0 lives in
/// [`TextureRecord::pixels`]).
#[derive(Debug, Clone)]
pub struct MipLevel {
    /// Level width in texels (the CreateTexture chain halves per level).
    pub width: u32,
    /// Level height in texels.
    pub height: u32,
    /// Texels in `0xAARRGGBB` order, row-major.
    pub pixels: Vec<u32>,
}

/// An offscreen render target (L6: `CreateRenderTarget`).
///
/// Owns its own host texel buffer, so `SetRenderTarget` redirects Clear and
/// draw output here instead of the implicit backbuffer. Texels are stored in
/// `0xAARRGGBB` order (the same convention as [`TextureRecord`]).
#[derive(Debug, Clone)]
pub struct RenderTargetRecord {
    /// The surface object's guest VA (also the `IDirect3DSurface9` pointer).
    pub handle: u64,
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// `D3DFMT_*` format (only A8R8G8B8 / X8R8G8B8 are accepted).
    pub format: u32,
    /// Texels in `0xAARRGGBB` order, row-major, top row first.
    pub pixels: Vec<u32>,
    /// Guest block VA handed out by the active `LockRect` (0 = not locked).
    pub locked_va: u64,
    /// Locked region (None = whole surface) in surface coordinates.
    pub locked_rect: Option<(i32, i32, i32, i32)>,
}

/// A D3D9 texture: host-owned texels plus the guest lock state.
///
/// Texels are stored in D3DCOLOR order (`0xAARRGGBB`, matching what the guest
/// writes through `LockRect`), row-major, top row first. The fragment stage
/// samples them and masks to 0RGB when writing the backbuffer. Level 0 is the
/// record's `width`/`height`/`pixels`; the halved chain lives in `mip_levels`
/// (empty when `levels == 1` — no mips).
#[derive(Debug, Clone)]
pub struct TextureRecord {
    /// The texture object's guest VA (also the `IDirect3DTexture9` pointer).
    pub handle: u64,
    /// Texture width in texels (level 0).
    pub width: u32,
    /// Texture height in texels (level 0).
    pub height: u32,
    /// Number of levels (slice: `levels == 0` → the full chain to 1×1).
    pub levels: u32,
    /// `D3DFMT_*` format (only A8R8G8B8 / X8R8G8B8 are accepted).
    pub format: u32,
    /// Level-0 texels in `0xAARRGGBB` order.
    pub pixels: Vec<u32>,
    /// Levels 1.. (the halved mip chain; empty for a single-level texture).
    pub mip_levels: Vec<MipLevel>,
    /// Surface object VA per level, handed out by `GetSurfaceLevel` (created
    /// lazily; 0 = that level's surface not created yet).
    pub surface_vas: Vec<u64>,
    /// Guest block VA handed out by the active `LockRect` (0 = not locked).
    pub locked_va: u64,
    /// The level the active `LockRect` targets (valid when `locked_va != 0`).
    pub locked_level: u32,
    /// Locked region (None = whole surface) in surface coordinates.
    pub locked_rect: Option<(i32, i32, i32, i32)>,
}

/// A D3D9 depth-stencil surface: host-owned depth texels.
///
/// Depth values are `f32` in `0.0 = near` .. `1.0 = far` (D3D9's cleared
/// default). `D3DFMT_D24S8` stores depth only — the stencil bits are unused
/// (documented; stencil operations are not modeled).
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
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "CreateTexture")?;
    let width_raw = read_arg(engine, ArgReg::Rdx, "CreateTexture")?;
    let height_raw = read_arg(engine, ArgReg::R8, "CreateTexture")?;
    let levels_raw = read_arg(engine, ArgReg::R9, "CreateTexture")?;
    let _usage = read_stack_argument(engine, 0x28, "CreateTexture Usage")?;
    let format_raw = read_stack_argument(engine, 0x30, "CreateTexture Format")?;
    let _pool = read_stack_argument(engine, 0x38, "CreateTexture Pool")?;
    let pp_texture = read_stack_argument(engine, 0x40, "CreateTexture ppTexture")?;
    let _shared_handle = read_stack_argument(engine, 0x48, "CreateTexture pSharedHandle")?;

    let width = low_u32(width_raw, "CreateTexture width")?;
    let height = low_u32(height_raw, "CreateTexture height")?;
    let levels_raw_value = low_u32(levels_raw, "CreateTexture levels")?;
    let format = low_u32(format_raw, "CreateTexture format")?;

    let valid = width > 0
        && height > 0
        && width <= MAX_TEXTURE_DIMENSION
        && height <= MAX_TEXTURE_DIMENSION
        && matches!(format, D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8)
        && pp_texture != 0;

    let return_value = if valid {
        let object = allocate_texture_object(engine, state)?;
        if object == 0 {
            D3DERR_INVALIDCALL
        } else {
            // L4 mip chain: `levels == 0` means the full chain down to 1×1;
            // an explicit count is clamped to the chain length (a request
            // beyond it is the honest clamp, not a silent 1-level texture).
            let chain_len = (width.max(height)).ilog2().saturating_add(1);
            let levels_requested = if levels_raw_value == 0 {
                chain_len
            } else {
                levels_raw_value
            };
            let levels = levels_requested.min(chain_len);
            let mip_levels = (1..levels)
                .map(|level| {
                    let level_w = (width >> level).max(1);
                    let level_h = (height >> level).max(1);
                    let texel_count =
                        usize::try_from(level_w.checked_mul(level_h).unwrap_or(0)).unwrap_or(0);
                    MipLevel {
                        width: level_w,
                        height: level_h,
                        pixels: vec![0; texel_count],
                    }
                })
                .collect();
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
                    mip_levels,
                    surface_vas: vec![0; usize::try_from(levels).unwrap_or(0)],
                    locked_va: 0,
                    locked_level: 0,
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

    ctx.finish(return_value)
}

/// Handles `IDirect3DTexture9::GetSurfaceLevel` (vtable slot 18).
///
/// L4: real level indexing — `level` must be inside the CreateTexture chain.
/// Each level gets its own lazily-created surface object (cached on the
/// record), so locking level 1 writes that level's texels, not level 0's.
pub fn handle_texture_get_surface_level(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "GetSurfaceLevel")?;
    let level_raw = read_arg(engine, ArgReg::Rdx, "GetSurfaceLevel")?;
    let pp_surface = read_arg(engine, ArgReg::R8, "GetSurfaceLevel")?;

    let level = low_u32(level_raw, "GetSurfaceLevel level")?;
    let valid_texture = state.d3d9().d3d9_textures.contains_key(&this_pointer);
    let return_value = if valid_texture && pp_surface != 0 {
        // Out-of-range level (or a degenerate record) → the honest invalid
        // call; a level the chain does not have must not alias level 0.
        let in_range = state
            .d3d9()
            .d3d9_textures
            .get(&this_pointer)
            .is_some_and(|record| level < record.levels);
        if !in_range {
            D3DERR_INVALIDCALL
        } else {
            let cached = state
                .d3d9()
                .d3d9_textures
                .get(&this_pointer)
                .and_then(|record| record.surface_vas.get(level as usize).copied())
                .unwrap_or(0);
            let surface = if cached != 0 {
                cached
            } else {
                let object = allocate_surface_object(engine, state)?;
                if object == 0 {
                    0
                } else {
                    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&this_pointer)
                        && let Some(slot) = record.surface_vas.get_mut(level as usize)
                    {
                        *slot = object;
                    }
                    state
                        .d3d9()
                        .d3d9_surface_textures
                        .insert(object, this_pointer);
                    state.d3d9().d3d9_surface_levels.insert(object, level);
                    object
                }
            };
            if surface == 0 {
                D3DERR_INVALIDCALL
            } else {
                write_guest_u64(engine, pp_surface, surface)
                    .context("failed to return IDirect3DSurface9 pointer")?;
                D3D_OK
            }
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Read a guest RECT (four i32s) at `rect_va`.
fn read_guest_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    rect_va: u64,
) -> Result<Option<(i32, i32, i32, i32)>> {
    if rect_va == 0 {
        return Ok(None);
    }
    let mut bytes = [0_u8; 16];
    engine
        .mem_read(rect_va, &mut bytes)
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
///
/// `level` selects which mip level the lock targets (the surface form resolves
/// it through [`D3D9State::d3d9_surface_levels`]; the texture form passes the
/// `RDX` level directly).
fn lock_rect_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    texture_va: u64,
    level: u32,
    p_locked_rect: u64,
    p_rect: u64,
) -> Result<u64> {
    let Some(record) = state.d3d9().d3d9_textures.get(&texture_va) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    // Level 0's dims are the record's; higher levels read the mip chain.
    let level_width = if level == 0 {
        record.width
    } else {
        record
            .mip_levels
            .get(level as usize - 1)
            .map_or(0, |mip| mip.width)
    };
    let level_height = if level == 0 {
        record.height
    } else {
        record
            .mip_levels
            .get(level as usize - 1)
            .map_or(0, |mip| mip.height)
    };
    if record.locked_va != 0 || level >= record.levels || level_width == 0 || level_height == 0 {
        return Ok(D3DERR_INVALIDCALL); // double lock / degenerate / bad level
    }
    let pitch = level_width
        .checked_mul(4)
        .context("texture pitch overflow")?;
    let total = u64::from(pitch)
        .checked_mul(u64::from(level_height))
        .context("texture lock size overflow")?;

    let rect = read_guest_rect(engine, p_rect)?;
    if let Some((left, top, right, bottom)) = rect {
        let within = left >= 0
            && top >= 0
            && right > left
            && bottom > top
            && right <= i32::try_from(level_width).unwrap_or(0)
            && bottom <= i32::try_from(level_height).unwrap_or(0);
        if !within {
            return Ok(D3DERR_INVALIDCALL);
        }
    }

    let block = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
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
            checked_address(p_locked_rect, 8, "D3DLOCKED_RECT.pBits"),
            p_bits,
        )
        .context("failed to write D3DLOCKED_RECT.pBits")?;
    }

    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
        record.locked_va = block;
        record.locked_level = level;
        record.locked_rect = rect;
    }
    Ok(D3D_OK)
}

/// LockRect body for an offscreen render target (L6): same guest-block
/// hand-off as the texture form, but the copy-back lands in the RT's own
/// texel buffer.
fn lock_rect_render_target(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    rt_va: u64,
    p_locked_rect: u64,
    p_rect: u64,
) -> Result<u64> {
    let Some(record) = state.d3d9().d3d9_render_targets.get(&rt_va) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    let (width, height) = (record.width, record.height);
    if record.locked_va != 0 || width == 0 || height == 0 {
        return Ok(D3DERR_INVALIDCALL); // double lock / degenerate
    }
    let pitch = width
        .checked_mul(4)
        .context("render-target pitch overflow")?;
    let total = u64::from(pitch)
        .checked_mul(u64::from(height))
        .context("render-target lock size overflow")?;

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

    let block = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if block == 0 {
        return Ok(D3DERR_INVALIDCALL); // allocation failed
    }
    // L6 read-back: LockRect exposes the surface's CURRENT texels (real
    // D3D9 semantics — the guest may read what was rendered). Copy the RT
    // content into the locked block; the guest's UnlockRect writes it back.
    let (left, top) = rect.map_or((0_i32, 0_i32), |r| (r.0, r.1));
    let row_width = usize::try_from(rect.map_or(width, |r| {
        u32::try_from(r.2.saturating_sub(r.0)).unwrap_or(0)
    }))
    .unwrap_or(0);
    for row in 0..height {
        let src_row = top.saturating_add(i32::try_from(row).unwrap_or(0));
        if src_row < 0 || src_row >= i32::try_from(height).unwrap_or(0) {
            continue;
        }
        let mut row_bytes = vec![0_u8; row_width.saturating_mul(4)];
        if let Some(record) = state.d3d9().d3d9_render_targets.get(&rt_va) {
            let row_start = usize::try_from(src_row)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(width).unwrap_or(0))
                .saturating_add(usize::try_from(left).unwrap_or(0));
            for (col, chunk) in row_bytes.chunks_exact_mut(4).enumerate() {
                let texel = record
                    .pixels
                    .get(row_start.saturating_add(col))
                    .copied()
                    .unwrap_or(0);
                chunk.copy_from_slice(&texel.to_le_bytes());
            }
        }
        let dst = block.saturating_add(
            u64::try_from(i64::from(row).saturating_mul(i64::from(pitch))).unwrap_or(0),
        );
        if engine.mem_write(dst, &row_bytes).is_err() {
            break;
        }
    }
    let offset = u64::try_from(i64::from(top).saturating_mul(i64::from(pitch)))
        .unwrap_or(0)
        .saturating_add(u64::try_from(i64::from(left).saturating_mul(4)).unwrap_or(0));
    let p_bits = block.saturating_add(offset);

    if p_locked_rect != 0 {
        write_guest_u32(engine, p_locked_rect, pitch)
            .context("failed to write D3DLOCKED_RECT.Pitch")?;
        write_guest_u64(
            engine,
            checked_address(p_locked_rect, 8, "D3DLOCKED_RECT.pBits"),
            p_bits,
        )
        .context("failed to write D3DLOCKED_RECT.pBits")?;
    }

    if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt_va) {
        record.locked_va = block;
        record.locked_rect = rect;
    }
    Ok(D3D_OK)
}

/// UnlockRect body for an offscreen render target: copy the locked guest
/// region back into the RT's texels and free the guest block.
fn unlock_rect_render_target(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    rt_va: u64,
) -> u64 {
    let Some(record) = state.d3d9().d3d9_render_targets.get(&rt_va) else {
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
            let mut texels: Vec<u32> = Vec::with_capacity(row_width);
            for chunk in row_bytes.chunks_exact(4) {
                let bytes: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
                texels.push(u32::from_le_bytes(bytes));
            }
            let row_start = usize::try_from(row)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(width).unwrap_or(0));
            if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt_va) {
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
    let _ = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .free_coherent(engine, locked_va);
    if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt_va) {
        record.locked_va = 0;
        record.locked_rect = None;
    }
    D3D_OK
}

/// Handles `IDirect3DSurface9::GetDesc` (vtable slot 12).
///
/// L6: real — writes the `D3DSURFACE_DESC` for a texture-view surface or an
/// offscreen render target. Layout (d3d9types.h): `Format @0`, `Type @4`
/// (`D3DRTYPE_SURFACE` = 3), `Usage @8`, `Pool @12`, `MultiSampleType @16`,
/// `MultiSampleQuality @20`, `Width @24`, `Height @28`.
pub fn handle_surface_get_desc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DSurface9::GetDesc")?;
    let desc_va = read_arg(engine, ArgReg::Rdx, "IDirect3DSurface9::GetDesc")?;

    let d3d = state.d3d9();
    let desc = d3d
        .d3d9_surface_textures
        .get(&this_pointer)
        .and_then(|texture_va| d3d.d3d9_textures.get(texture_va))
        .map(|record| (record.format, record.width, record.height))
        .or_else(|| {
            d3d.d3d9_render_targets
                .get(&this_pointer)
                .map(|record| (record.format, record.width, record.height))
        });

    let return_value = if let Some((format, width, height)) = desc
        && desc_va != 0
    {
        let mut bytes = [0_u8; 32];
        bytes[0..4].copy_from_slice(&format.to_le_bytes());
        bytes[4..8].copy_from_slice(&D3DRTYPE_SURFACE.to_le_bytes());
        bytes[16..20].copy_from_slice(&0_u32.to_le_bytes()); // MultiSampleType: NONE
        bytes[20..24].copy_from_slice(&0_u32.to_le_bytes()); // MultiSampleQuality: 0
        bytes[24..28].copy_from_slice(&width.to_le_bytes());
        bytes[28..32].copy_from_slice(&height.to_le_bytes());
        if engine.mem_write(desc_va, &bytes).is_ok() {
            D3D_OK
        } else {
            D3DERR_INVALIDCALL
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DSurface9::LockRect` (vtable slot 13).
pub fn handle_surface_lock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DSurface9::LockRect")?;
    let p_locked_rect = read_arg(engine, ArgReg::Rdx, "IDirect3DSurface9::LockRect")?;
    let p_rect = read_arg(engine, ArgReg::R8, "IDirect3DSurface9::LockRect")?;
    let _flags = read_stack_argument(engine, 0x28, "LockRect Flags")?;

    let texture_va = state
        .d3d9()
        .d3d9_surface_textures
        .get(&this_pointer)
        .copied()
        .unwrap_or(0);
    // The surface resolves its mip level through the surface→level map.
    let level = state
        .d3d9()
        .d3d9_surface_levels
        .get(&this_pointer)
        .copied()
        .unwrap_or(0);
    let return_value = if texture_va != 0 {
        lock_rect_common(engine, state, texture_va, level, p_locked_rect, p_rect)?
    } else if state.d3d9().d3d9_render_targets.contains_key(&this_pointer) {
        lock_rect_render_target(engine, state, this_pointer, p_locked_rect, p_rect)?
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DTexture9::LockRect` (vtable slot 19) — the deprecated
/// texture-level form; `RDX` is the mip level.
pub fn handle_texture_lock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DTexture9::LockRect")?;
    let level_raw = read_arg(engine, ArgReg::Rdx, "IDirect3DTexture9::LockRect")?;
    let p_locked_rect = read_arg(engine, ArgReg::R8, "IDirect3DTexture9::LockRect")?;
    let p_rect = read_arg(engine, ArgReg::R9, "IDirect3DTexture9::LockRect")?;
    let _flags = read_stack_argument(engine, 0x28, "IDirect3DTexture9::LockRect Flags")?;

    let level = low_u32(level_raw, "IDirect3DTexture9::LockRect level")?;
    let return_value = lock_rect_common(engine, state, this_pointer, level, p_locked_rect, p_rect)?;

    ctx.finish(return_value)
}

/// Shared UnlockRect body: copy the locked region back into the host texels
/// of the locked level and free the guest block.
fn unlock_rect_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    texture_va: u64,
) -> u64 {
    let Some(record) = state.d3d9().d3d9_textures.get(&texture_va) else {
        return D3DERR_INVALIDCALL;
    };
    let locked_va = record.locked_va;
    let locked_level = record.locked_level;
    if locked_va == 0 {
        return D3DERR_INVALIDCALL; // not locked
    }
    // The locked level's dims (level 0 = the record's own texels).
    let (width, height) = if locked_level == 0 {
        (record.width, record.height)
    } else {
        record
            .mip_levels
            .get(locked_level as usize - 1)
            .map_or((0, 0), |mip| (mip.width, mip.height))
    };
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
            // Copy the row into the level's host texels (D3DCOLOR byte order).
            let mut texels: Vec<u32> = Vec::with_capacity(row_width);
            for chunk in row_bytes.chunks_exact(4) {
                let bytes: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
                texels.push(u32::from_le_bytes(bytes));
            }
            let row_start = usize::try_from(row)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(width).unwrap_or(0));
            for (col, texel) in texels.into_iter().enumerate() {
                let index = row_start
                    .saturating_add(usize::try_from(left).unwrap_or(0))
                    .saturating_add(col);
                if locked_level == 0 {
                    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va)
                        && let Some(slot) = record.pixels.get_mut(index)
                    {
                        *slot = texel;
                    }
                } else if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va)
                    && let Some(mip) = record.mip_levels.get_mut(locked_level as usize - 1)
                    && let Some(slot) = mip.pixels.get_mut(index)
                {
                    *slot = texel;
                }
            }
        }
    }
    let _ = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .free_coherent(engine, locked_va);
    if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
        record.locked_va = 0;
        record.locked_level = 0;
        record.locked_rect = None;
    }
    D3D_OK
}

/// Handles `IDirect3DSurface9::UnlockRect` (vtable slot 14).
pub fn handle_surface_unlock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DSurface9::UnlockRect")?;

    let texture_va = state
        .d3d9()
        .d3d9_surface_textures
        .get(&this_pointer)
        .copied()
        .unwrap_or(0);
    let return_value = if texture_va != 0 {
        unlock_rect_common(engine, state, texture_va)
    } else if state.d3d9().d3d9_render_targets.contains_key(&this_pointer) {
        unlock_rect_render_target(engine, state, this_pointer)
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DTexture9::UnlockRect` (vtable slot 20).
pub fn handle_texture_unlock_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DTexture9::UnlockRect")?;

    let return_value = unlock_rect_common(engine, state, this_pointer);

    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetTexture` (vtable slot 65).
pub fn handle_set_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetTexture")?;
    let stage_raw = read_arg(engine, ArgReg::Rdx, "SetTexture")?;
    let texture_va = read_arg(engine, ArgReg::R8, "SetTexture")?;

    let stage = low_u32(stage_raw, "SetTexture stage")?;
    if let Some(slot) = state
        .d3d9()
        .d3d9_texture_bindings
        .get_mut(usize::try_from(stage).unwrap_or(usize::MAX))
    {
        *slot = texture_va;
    }

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetTexture` (vtable slot 64).
pub fn handle_get_texture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetTexture")?;
    let stage_raw = read_arg(engine, ArgReg::Rdx, "GetTexture")?;
    let pp_texture = read_arg(engine, ArgReg::R8, "GetTexture")?;

    let stage = low_u32(stage_raw, "GetTexture stage")?;
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

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetTextureStageState` (vtable slot 66).
pub fn handle_get_texture_stage_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetTextureStageState")?;
    let stage_raw = read_arg(engine, ArgReg::Rdx, "GetTextureStageState")?;
    let state_type_raw = read_arg(engine, ArgReg::R8, "GetTextureStageState")?;
    let p_value = read_arg(engine, ArgReg::R9, "GetTextureStageState")?;

    let stage = low_u32(stage_raw, "GetTextureStageState stage")?;
    let state_type = low_u32(state_type_raw, "GetTextureStageState state type")?;
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

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetSamplerState` (vtable slot 68).
pub fn handle_get_sampler_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetSamplerState")?;
    let sampler_raw = read_arg(engine, ArgReg::Rdx, "GetSamplerState")?;
    let state_type_raw = read_arg(engine, ArgReg::R8, "GetSamplerState")?;
    let p_value = read_arg(engine, ArgReg::R9, "GetSamplerState")?;

    let sampler = low_u32(sampler_raw, "GetSamplerState sampler")?;
    let state_type = low_u32(state_type_raw, "GetSamplerState state type")?;
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

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DTexture9::Release` (vtable slot 2).
///
/// Frees every per-level surface object, the active lock block, and the
/// texture record (the level texels drop with it).
pub fn handle_texture_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DTexture9::Release")?;

    let (surface_vas, locked_va) = state
        .d3d9()
        .d3d9_textures
        .get(&this_pointer)
        .map_or((Vec::new(), 0), |r| (r.surface_vas.clone(), r.locked_va));
    let exists = !surface_vas.is_empty()
        || locked_va != 0
        || state.d3d9().d3d9_textures.contains_key(&this_pointer);

    let return_value = if exists {
        // Unbind from every stage.
        for slot in &mut state.d3d9().d3d9_texture_bindings {
            if *slot == this_pointer {
                *slot = 0;
            }
        }
        for surface_va in surface_vas {
            if surface_va == 0 {
                continue;
            }
            state.d3d9().d3d9_surface_textures.remove(&surface_va);
            state.d3d9().d3d9_surface_levels.remove(&surface_va);
            let vtable = surface_va.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, vtable);
        }
        if locked_va != 0 {
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, locked_va);
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DTEXTURE9_OBJECT_OFFSET);
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, vtable);
        state.d3d9().d3d9_textures.remove(&this_pointer);
        1
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DSurface9::Release` (vtable slot 2).
///
/// Releases the surface view only — the texture record (and its texels)
/// survive until the texture itself is released.
pub fn handle_surface_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DSurface9::Release")?;

    let return_value =
        if let Some(texture_va) = state.d3d9().d3d9_surface_textures.remove(&this_pointer) {
            // Drop the surface view for its level (the record's texels —
            // including every other level — survive until the texture
            // itself is released).
            let level = state
                .d3d9()
                .d3d9_surface_levels
                .remove(&this_pointer)
                .unwrap_or(0);
            if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va)
                && let Some(slot) = record.surface_vas.get_mut(level as usize)
            {
                *slot = 0;
            }
            let vtable = this_pointer.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, vtable);
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
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, vtable);
            1
        } else if state
            .d3d9()
            .d3d9_render_targets
            .remove(&this_pointer)
            .is_some()
        {
            // L6 render target: drop the record (and unbind if it is the
            // bound slot-0 target — the backbuffer default is restored).
            if state.d3d9().d3d9_render_target == this_pointer {
                state.d3d9().d3d9_render_target = 0;
            }
            let vtable = this_pointer.saturating_sub(IDIRECT3DSURFACE9_OBJECT_OFFSET);
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, vtable);
            1
        } else {
            0
        };

    ctx.finish(return_value)
}

/// Handles `IDirect3DTexture9::GetLevelCount` (vtable slot 13).
pub fn handle_texture_get_level_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "GetLevelCount")?;

    let levels = state
        .d3d9()
        .d3d9_textures
        .get(&this_pointer)
        .map_or(0, |r| r.levels);
    let return_value = u64::from(levels);

    ctx.finish(return_value)
}
