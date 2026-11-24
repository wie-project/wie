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
///
/// The texels are an `Arc` for the same sharing as [`TextureRecord::pixels`]:
/// a captured draw takes a refcount bump per level instead of a copy.
#[derive(Debug, Clone)]
pub struct MipLevel {
    /// Level width in texels (the CreateTexture chain halves per level).
    pub width: u32,
    /// Level height in texels.
    pub height: u32,
    /// Texels in `0xAARRGGBB` order, row-major.
    pub pixels: std::sync::Arc<Vec<u32>>,
}

/// An offscreen render target (L6: `CreateRenderTarget`).
///
/// Owns its own host texel buffer, so `SetRenderTarget` redirects Clear and
/// draw output here instead of the implicit backbuffer. Texels are stored in
/// `0xAARRGGBB` order (the same convention as [`TextureRecord`]).
///
/// The texel storage is an `Arc` (Wave 2 capture): the capture stream hands
/// the render thread `Arc` clones of the current texels at each flush and
/// installs the render thread's post-frame copy back through the handback
/// slot, so every cross-thread transfer is a refcount bump. Mutators take the
/// buffer exclusively with `Arc::make_mut` (a free clone when the refcount is
/// 1 — the legacy in-handler path).
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
    pub pixels: std::sync::Arc<Vec<u32>>,
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
///
/// The texel storage is an `Arc` for the same reason as
/// [`RenderTargetRecord::pixels`]: the capture stream hands the render thread
/// `Arc` clones of the bound stages' texels at record time, so a draw costs a
/// refcount bump per level instead of a copy of the whole mip chain. The
/// writer (`UnlockRect`'s copy-back) takes the level exclusively with
/// `Arc::make_mut`, so a recorded draw keeps the texels it drew and the copy
/// is free when no snapshot is live.
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
    pub pixels: std::sync::Arc<Vec<u32>>,
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
///
/// The storage is an `Arc` for the same Wave 2 capture round-trip as
/// [`RenderTargetRecord::pixels`] (see that doc).
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
    pub depth: std::sync::Arc<Vec<f32>>,
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
            // Q9/C: pooled texture mip chains — reuse old chain Vecs (capacity
            // retained) to avoid per-frame realloc. The pool is
            // `HashMap<tex_id, Vec<Vec<u32>>>` per device.
            let mut pooled_chain: Option<Vec<Vec<u32>>> = None;
            if !state.d3d9().d3d9_texture_pool.is_empty()
                && let Some(key) = state.d3d9().d3d9_texture_pool.keys().next().copied()
            {
                pooled_chain = state.d3d9().d3d9_texture_pool.remove(&key);
            }
            let (pixels, mip_levels) = if let Some(mut chain) = pooled_chain {
                // chain[0] is level0, chain[1..] are mips — reuse capacity.
                let texel_count =
                    usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
                let mut pixels = if !chain.is_empty() {
                    let mut v = chain.remove(0);
                    v.clear();
                    v.resize(texel_count, 0);
                    v
                } else {
                    vec![0; texel_count]
                };
                let mut mip_levels: Vec<MipLevel> =
                    Vec::with_capacity((levels.saturating_sub(1)) as usize);
                for level in 1..levels {
                    let level_w = (width >> level).max(1);
                    let level_h = (height >> level).max(1);
                    let needed =
                        usize::try_from(level_w.checked_mul(level_h).unwrap_or(0)).unwrap_or(0);
                    let vec = if !chain.is_empty() {
                        let mut v = chain.remove(0);
                        v.clear();
                        v.resize(needed, 0);
                        v
                    } else {
                        vec![0; needed]
                    };
                    mip_levels.push(MipLevel {
                        width: level_w,
                        height: level_h,
                        pixels: std::sync::Arc::new(vec),
                    });
                }
                // Return any leftover Vecs to the pool (capacity retained) so
                // we don't discard reusable allocations when chain lengths differ.
                if !chain.is_empty() {
                    let mut new_key = object.wrapping_add(1);
                    while state.d3d9().d3d9_texture_pool.contains_key(&new_key) {
                        new_key = new_key.wrapping_add(1);
                    }
                    state.d3d9().d3d9_texture_pool.insert(new_key, chain);
                }
                let _ = &mut pixels;
                (pixels, mip_levels)
            } else {
                let mip_levels = (1..levels)
                    .map(|level| {
                        let level_w = (width >> level).max(1);
                        let level_h = (height >> level).max(1);
                        let texel_count =
                            usize::try_from(level_w.checked_mul(level_h).unwrap_or(0)).unwrap_or(0);
                        MipLevel {
                            width: level_w,
                            height: level_h,
                            pixels: std::sync::Arc::new(vec![0; texel_count]),
                        }
                    })
                    .collect();
                let texel_count =
                    usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
                (vec![0; texel_count], mip_levels)
            };
            state.d3d9().d3d9_textures.insert(
                object,
                TextureRecord {
                    handle: object,
                    width,
                    height,
                    levels,
                    format,
                    pixels: std::sync::Arc::new(pixels),
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

/// Encode one scanline of host texels into `dst` as `0xAARRGGBB` little-endian
/// bytes. Texels past the end of `pixels` encode as 0, so a lock block the
/// record does not fully cover still reads as transparent black.
///
/// `dst` is whole texels (`len % 4 == 0`); every byte is written, which is what
/// lets the callers reuse one staging buffer across scanlines.
fn encode_scanline(pixels: &[u32], start: usize, dst: &mut [u8]) {
    for (col, chunk) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let texel = pixels.get(start.saturating_add(col)).copied().unwrap_or(0);
        chunk.copy_from_slice(&texel.to_le_bytes());
    }
}

/// Decode one scanline of `0xAARRGGBB` little-endian guest bytes into host
/// texels, starting at `start`. Stops at the end of `pixels`, so a clipped row
/// never writes past the level's texels.
fn decode_scanline(pixels: &mut [u32], start: usize, bytes: &[u8]) {
    for (col, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let Some(slot) = pixels.get_mut(start.saturating_add(col)) else {
            break;
        };
        *slot = u32::from_le_bytes(*chunk);
    }
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
    // Wave 2 capture sync point: the replay runs on the render thread at
    // flush boundaries, so a guest that renders into this RT and locks it
    // BEFORE any Present needs the pending ops replayed RIGHT NOW — flush
    // + rendezvous-wait, then install the handback (see
    // `capture::sync_flush_for_target_read`).
    if super::capture::capture_enabled(state) {
        super::capture::sync_flush_for_target_read(state);
    }
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
    // One `pixels` borrow for the whole read-back (the per-row map lookup is
    // gone) and, on the row path below, one staging buffer instead of a fresh
    // `vec!` per scanline: O(height) allocations became O(1) — 768 × 4 KB per
    // 1024×768 lock before this.
    let pixels: &[u32] = state
        .d3d9()
        .d3d9_render_targets
        .get(&rt_va)
        .map_or(&[], |record| record.pixels.as_slice());
    let width_us = usize::try_from(width).unwrap_or(0);
    let left_us = usize::try_from(left).unwrap_or(0);
    let row_len = row_width.saturating_mul(4);
    // Fast path: a full-width rect from row 0 spans contiguous texels AND
    // contiguous guest bytes, so `host_slice_mut` borrows the whole block and
    // the rect lands in one pass — no staging copy, no `mem_write` per row. A
    // span crossing arenas (or executable/denied) yields `None` and falls
    // through to the per-row path below.
    let span = usize::try_from(total).unwrap_or(0);
    let mut copied = false;
    if row_width == width_us
        && left == 0
        && top == 0
        && span != 0
        && let Some(whole) = engine.host_slice_mut(block, span)
    {
        let pitch_us = usize::try_from(pitch).unwrap_or(0);
        for row in 0..height {
            let row_us = usize::try_from(row).unwrap_or(0);
            let offset = row_us.saturating_mul(pitch_us);
            let Some(scanline) = whole.get_mut(offset..offset.saturating_add(row_len)) else {
                break;
            };
            encode_scanline(pixels, row_us.saturating_mul(width_us), scanline);
        }
        copied = true;
    }
    if !copied {
        let mut row_bytes = vec![0_u8; row_len];
        for row in 0..height {
            let src_row = top.saturating_add(i32::try_from(row).unwrap_or(0));
            if src_row < 0 || src_row >= i32::try_from(height).unwrap_or(0) {
                continue;
            }
            let row_start = usize::try_from(src_row)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(left_us);
            encode_scanline(pixels, row_start, &mut row_bytes);
            let dst = block.saturating_add(
                u64::try_from(i64::from(row).saturating_mul(i64::from(pitch))).unwrap_or(0),
            );
            if engine.mem_write(dst, &row_bytes).is_err() {
                break;
            }
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
    // Wave 2 capture sync point: install a pending handback before the guest
    // writes land, so they layer on top of the render thread's latest state.
    if super::capture::capture_enabled(state) {
        super::capture::drain_handback(state);
    }
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
        let left_us = usize::try_from(left).unwrap_or(0);
        let width_us = usize::try_from(width).unwrap_or(0);
        let pitch_us = usize::try_from(pitch).unwrap_or(0);
        let row_len = row_width.saturating_mul(4);
        // One scanline of guest bytes lands in the RT's texels at `row`. The
        // decode is inline: the per-scanline `Vec<u32>` this replaces was one
        // allocation per row (768 of them for a 1024×768 lock).
        let write_row = |state: &mut WinApiState, row: i32, bytes: &[u8]| {
            let row_start = usize::try_from(row).unwrap_or(0).saturating_mul(width_us);
            if let Some(record) = state.d3d9().d3d9_render_targets.get_mut(&rt_va) {
                let pixels = std::sync::Arc::make_mut(&mut record.pixels);
                decode_scanline(pixels, row_start.saturating_add(left_us), bytes);
            }
        };
        // Fast path: for a full-width rect `pitch == row_len`, so the rect's
        // rows are contiguous in the locked block and one `host_slice` borrow
        // covers all of them — no per-scanline `mem_read`, no staging buffer.
        // A span crossing arenas yields `None` and falls through to the rows.
        let mut copied = false;
        if row_width == width_us && left == 0 {
            let rows = usize::try_from(bottom.saturating_sub(top)).unwrap_or(0);
            let span = rows.saturating_mul(pitch_us);
            let top_va = locked_va.saturating_add(
                u64::try_from(usize::try_from(top).unwrap_or(0).saturating_mul(pitch_us))
                    .unwrap_or(0),
            );
            if span != 0
                && let Some(src) = engine.host_slice(top_va, span)
            {
                for row in top..bottom {
                    let offset = usize::try_from(row.saturating_sub(top))
                        .unwrap_or(0)
                        .saturating_mul(pitch_us);
                    let Some(scanline) = src.get(offset..offset.saturating_add(row_len)) else {
                        break;
                    };
                    write_row(state, row, scanline);
                }
                copied = true;
            }
        }
        if !copied {
            let mut row_bytes = vec![0_u8; row_len];
            for row in top..bottom {
                let src = locked_va
                    .saturating_add(
                        u64::try_from(
                            i64::from(row).saturating_mul(i64::try_from(pitch).unwrap_or(0)),
                        )
                        .unwrap_or(0),
                    )
                    .saturating_add(u64::try_from(i64::from(left).saturating_mul(4)).unwrap_or(0));
                if engine.mem_read(src, &mut row_bytes).is_err() {
                    continue;
                }
                write_row(state, row, &row_bytes);
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
    // Wave 2 capture: the copy-back made the emu thread's texels the
    // authority for this target — mark it so the next flush sends them to
    // the render thread as an input.
    if super::capture::capture_enabled(state) {
        state.d3d9().d3d9_capture_emu_dirty_rt.insert(rt_va);
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
        let left_us = usize::try_from(left).unwrap_or(0);
        let width_us = usize::try_from(width).unwrap_or(0);
        let pitch_us = usize::try_from(pitch).unwrap_or(0);
        let row_len = row_width.saturating_mul(4);
        // Copy one scanline into the locked level's host texels (D3DCOLOR byte
        // order). Inline decode: the per-scanline `Vec<u32>` this replaces was
        // one allocation per row.
        let write_row = |state: &mut WinApiState, row: i32, bytes: &[u8]| {
            let start = usize::try_from(row)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(left_us);
            if locked_level == 0 {
                if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va) {
                    let pixels = std::sync::Arc::make_mut(&mut record.pixels);
                    decode_scanline(pixels, start, bytes);
                }
            } else if let Some(record) = state.d3d9().d3d9_textures.get_mut(&texture_va)
                && let Some(mip) = record.mip_levels.get_mut(locked_level as usize - 1)
            {
                let pixels = std::sync::Arc::make_mut(&mut mip.pixels);
                decode_scanline(pixels, start, bytes);
            }
        };
        // Fast path: as in the render-target form, a full-width rect's rows are
        // contiguous in the locked block (`pitch == row_len`), so one
        // `host_slice` borrow covers the rect. `None` falls back per row.
        let mut copied = false;
        if row_width == width_us && left == 0 {
            let rows = usize::try_from(bottom.saturating_sub(top)).unwrap_or(0);
            let span = rows.saturating_mul(pitch_us);
            let top_va = locked_va.saturating_add(
                u64::try_from(usize::try_from(top).unwrap_or(0).saturating_mul(pitch_us))
                    .unwrap_or(0),
            );
            if span != 0
                && let Some(src) = engine.host_slice(top_va, span)
            {
                for row in top..bottom {
                    let offset = usize::try_from(row.saturating_sub(top))
                        .unwrap_or(0)
                        .saturating_mul(pitch_us);
                    let Some(scanline) = src.get(offset..offset.saturating_add(row_len)) else {
                        break;
                    };
                    write_row(state, row, scanline);
                }
                copied = true;
            }
        }
        if !copied {
            let mut row_bytes = vec![0_u8; row_len];
            for row in top..bottom {
                let src = locked_va
                    .saturating_add(
                        u64::try_from(
                            i64::from(row).saturating_mul(i64::try_from(pitch).unwrap_or(0)),
                        )
                        .unwrap_or(0),
                    )
                    .saturating_add(u64::try_from(i64::from(left).saturating_mul(4)).unwrap_or(0));
                if engine.mem_read(src, &mut row_bytes).is_err() {
                    continue;
                }
                write_row(state, row, &row_bytes);
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

    // Q9/C: pull the record so we can return its mip chain Vecs to the
    // per-device pool (capacity retained, no per-frame realloc) instead of
    // dropping them.
    let record = state.d3d9().d3d9_textures.remove(&this_pointer);
    let return_value = if let Some(record) = record {
        let surface_vas = record.surface_vas.clone();
        let locked_va = record.locked_va;
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
        // Q9/C: per-device pool `HashMap<tex_id, Vec<Vec<u32>>>` — retain
        // capacities by moving the chain Vecs into the pool. A level still
        // shared with a recorded draw (`Arc` count > 1) is copied instead:
        // the pool owns plain `Vec`s, and dropping the snapshot to avoid the
        // copy would mutate what that draw draws.
        let mut chain: Vec<Vec<u32>> = Vec::with_capacity(record.levels as usize);
        chain.push(std::sync::Arc::unwrap_or_clone(record.pixels));
        for mip in record.mip_levels {
            chain.push(std::sync::Arc::unwrap_or_clone(mip.pixels));
        }
        state.d3d9().d3d9_texture_pool.insert(this_pointer, chain);
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
            // Wave 2 capture: the render thread's copy of this VA must be
            // invalidated (a future surface can reuse the VA with new dims).
            state.d3d9().d3d9_capture_depth_seen.remove(&this_pointer);
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
            // Wave 2 capture: invalidate the render thread's copy bookkeeping
            // for this VA (a future surface can reuse the VA with new dims).
            state.d3d9().d3d9_capture_rt_seen.remove(&this_pointer);
            state.d3d9().d3d9_capture_emu_dirty_rt.remove(&this_pointer);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::winapi_state_default_with_bump_heap;
    use wie_cpu::{CpuEngine, IcedCpu};

    /// The render-target handle (a `d3d9_render_targets` key, never deref'd).
    const RT_VA: u64 = 0x9000_0000;
    /// The texture handle (a `d3d9_textures` key, never deref'd).
    const TEXTURE_VA: u64 = 0x9000_1000;
    /// Guest scratch below the fixture's bump heap (`[0x2000, 0x10000)`): the
    /// `D3DLOCKED_RECT` the lock form writes, and the `RECT` it reads.
    const LOCKED_RECT_VA: u64 = 0x1000;
    const RECT_VA: u64 = 0x1080;
    /// The fixture heap's guest control block (`attach_guest_control(0x2000)`),
    /// and the bump cursor the tests seed past that block.
    const HEAP_CTRL_VA: u64 = 0x2000;
    const HEAP_BUMP_VA: u64 = 0x2100;

    /// Guest memory for the copy loops: 1 MB of RW at `0x1000`, which covers
    /// both the scratch VAs and the bump heap the lock blocks come from.
    ///
    /// Read+write, not `ALL`: `host_slice_mut` refuses executable spans (a
    /// store to code has to go through the SMC path), so an executable mapping
    /// would send every write-back down the per-row fallback and leave the
    /// whole-block path untested.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::READ_WRITE)
            .expect("map test memory");
        cpu
    }

    /// Engine + state with the fixture heap's bump cursor seeded the way a real
    /// guest seeds it: `alloc_coherent` pulls the cursor out of the guest
    /// control block, and a fresh mapping reads as 0 — not a valid bump.
    fn setup() -> (IcedCpu, WinApiState) {
        let mut engine = test_engine();
        write_guest(
            &mut engine,
            HEAP_CTRL_VA.saturating_add(wie_cpu::guest_layout::HEAP_CTRL_BUMP_OFFSET),
            &HEAP_BUMP_VA.to_le_bytes(),
        );
        (engine, winapi_state_default_with_bump_heap())
    }

    fn read_guest(engine: &mut IcedCpu, address: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(address, &mut bytes).expect("guest read");
        bytes
    }

    fn write_guest(engine: &mut IcedCpu, address: u64, bytes: &[u8]) {
        engine.mem_write(address, bytes).expect("guest write");
    }

    /// Texels as the guest sees them through a lock: `0xAARRGGBB` little-endian.
    fn texels_to_bytes(texels: &[u32]) -> Vec<u8> {
        texels
            .iter()
            .flat_map(|texel| texel.to_le_bytes())
            .collect()
    }

    /// `D3DLOCKED_RECT` as the handler left it: `(Pitch, pBits)`.
    fn locked_rect_of(engine: &mut IcedCpu) -> (u32, u64) {
        let pitch = read_guest(engine, LOCKED_RECT_VA, 4)
            .try_into()
            .expect("4 bytes");
        let p_bits = read_guest(engine, LOCKED_RECT_VA.saturating_add(8), 8)
            .try_into()
            .expect("8 bytes");
        (u32::from_le_bytes(pitch), u64::from_le_bytes(p_bits))
    }

    /// Write a `RECT` for the next `LockRect` (four `LONG`s).
    fn put_rect(engine: &mut IcedCpu, rect: (i32, i32, i32, i32)) {
        let (left, top, right, bottom) = rect;
        let mut bytes = Vec::with_capacity(16);
        for value in [left, top, right, bottom] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        write_guest(engine, RECT_VA, &bytes);
    }

    fn insert_render_target(state: &mut WinApiState, width: u32, height: u32, texels: Vec<u32>) {
        state.d3d9().d3d9_render_targets.insert(
            RT_VA,
            RenderTargetRecord {
                handle: RT_VA,
                width,
                height,
                format: D3DFMT_A8R8G8B8,
                pixels: std::sync::Arc::new(texels),
                locked_va: 0,
                locked_rect: None,
            },
        );
    }

    fn insert_texture(state: &mut WinApiState, width: u32, height: u32, texels: Vec<u32>) {
        state.d3d9().d3d9_textures.insert(
            TEXTURE_VA,
            TextureRecord {
                handle: TEXTURE_VA,
                width,
                height,
                levels: 1,
                format: D3DFMT_A8R8G8B8,
                pixels: std::sync::Arc::new(texels),
                mip_levels: Vec::new(),
                surface_vas: Vec::new(),
                locked_va: 0,
                locked_level: 0,
                locked_rect: None,
            },
        );
    }

    fn render_target_texels(state: &mut WinApiState) -> Vec<u32> {
        state
            .d3d9()
            .d3d9_render_targets
            .get(&RT_VA)
            .map(|record| record.pixels.as_ref().clone())
            .expect("render target record")
    }

    fn texture_texels(state: &mut WinApiState) -> Vec<u32> {
        state
            .d3d9()
            .d3d9_textures
            .get(&TEXTURE_VA)
            .map(|record| record.pixels.as_ref().clone())
            .expect("texture record")
    }

    /// `0xFF00_0000 | index`: every texel distinct, so a misplaced write shows.
    fn sample_texels(count: u32) -> Vec<u32> {
        (0..count).map(|index| 0xFF00_0000 | index).collect()
    }

    #[test]
    fn render_target_lock_round_trips_a_full_surface() {
        let (mut engine, mut state) = setup();
        let texels = sample_texels(12); // 4x3
        insert_render_target(&mut state, 4, 3, texels.clone());

        let result = lock_rect_render_target(&mut engine, &mut state, RT_VA, LOCKED_RECT_VA, 0)
            .expect("lock read-back runs");
        assert_eq!(result, D3D_OK, "LockRect(NULL rect) succeeds");

        let (pitch, p_bits) = locked_rect_of(&mut engine);
        assert_eq!(pitch, 16, "pitch is width * 4");
        assert_eq!(
            read_guest(&mut engine, p_bits, 48),
            texels_to_bytes(&texels),
            "a full-surface lock exposes every texel, row by row"
        );

        // The guest edits the surface through the pointer it was given.
        let mut edited = texels.clone();
        edited[0] = 0x1122_3344;
        edited[11] = 0x5566_7788;
        write_guest(&mut engine, p_bits, &texels_to_bytes(&edited));

        assert_eq!(
            unlock_rect_render_target(&mut engine, &mut state, RT_VA),
            D3D_OK,
            "UnlockRect succeeds"
        );
        assert_eq!(
            render_target_texels(&mut state),
            edited,
            "the guest's edits land back in the texels"
        );
    }

    #[test]
    fn render_target_unlock_writes_back_only_the_clipped_rect() {
        let (mut engine, mut state) = setup();
        let texels = sample_texels(12); // 4x3
        insert_render_target(&mut state, 4, 3, texels.clone());

        // Columns 1..3, rows 0..2: the span is two texels (8 bytes) of a
        // sixteen-byte row, so only the clipped span of each row may move.
        let rect = (1, 0, 3, 2);
        put_rect(&mut engine, rect);
        let result =
            lock_rect_render_target(&mut engine, &mut state, RT_VA, LOCKED_RECT_VA, RECT_VA)
                .expect("clipped lock runs");
        assert_eq!(result, D3D_OK);

        let (pitch, p_bits) = locked_rect_of(&mut engine);
        assert_eq!(pitch, 16, "the clipped rect keeps the surface pitch");
        // The read-back writes each row's clipped span at that row's start in
        // the locked block, so the block base is pBits minus the rect origin.
        let block = p_bits.saturating_sub(4); // one texel of `left` offset
        for row in 0..3_usize {
            let first = row.saturating_mul(4).saturating_add(1);
            let span = texels.get(first..first + 2).expect("span in range");
            let at = block.saturating_add(u64::try_from(row).unwrap_or(0).saturating_mul(16));
            assert_eq!(
                read_guest(&mut engine, at, 8),
                texels_to_bytes(span),
                "row {row} copies the clipped span and no neighbouring texel"
            );
        }

        // Both rect rows edited through the pointer the guest was given: the
        // write-back must own exactly those texels.
        let mut expected = texels.clone();
        expected[1] = 0xAABB_CCDD;
        expected[2] = 0x0011_2233;
        expected[5] = 0x4455_6677;
        expected[6] = 0x8899_AABB;
        write_guest(
            &mut engine,
            p_bits,
            &texels_to_bytes(&[expected[1], expected[2]]),
        );
        write_guest(
            &mut engine,
            p_bits.saturating_add(16),
            &texels_to_bytes(&[expected[5], expected[6]]),
        );

        assert_eq!(
            unlock_rect_render_target(&mut engine, &mut state, RT_VA),
            D3D_OK
        );
        assert_eq!(
            render_target_texels(&mut state),
            expected,
            "rows outside the rect keep their texels"
        );
    }

    #[test]
    fn render_target_lock_honours_a_pitch_wider_than_the_rect() {
        let (mut engine, mut state) = setup();
        let texels = sample_texels(12); // 4x3
        insert_render_target(&mut state, 4, 3, texels.clone());

        // Columns 0..2 of every row: the span is 8 bytes wide, the pitch 16 —
        // the geometry that rules out a contiguous whole-rect copy.
        let rect = (0, 0, 2, 3);
        put_rect(&mut engine, rect);
        let result =
            lock_rect_render_target(&mut engine, &mut state, RT_VA, LOCKED_RECT_VA, RECT_VA)
                .expect("narrow lock runs");
        assert_eq!(result, D3D_OK);

        let (pitch, p_bits) = locked_rect_of(&mut engine);
        let row_width = u32::try_from(rect.2 - rect.0).unwrap_or(0);
        assert_eq!(pitch, 16);
        assert_ne!(
            pitch,
            row_width * 4,
            "the rect is narrower than a surface row"
        );

        // Consecutive rect rows sit one surface pitch apart in the block.
        for row in 0..3_usize {
            let first = row.saturating_mul(4);
            let span = texels.get(first..first + 2).expect("span in range");
            let at = p_bits.saturating_add(u64::try_from(row).unwrap_or(0).saturating_mul(16));
            assert_eq!(
                read_guest(&mut engine, at, 8),
                texels_to_bytes(span),
                "rect row {row} starts one pitch after the previous row"
            );
        }

        let mut expected = texels.clone();
        expected[1] = 0x0F0E_0D0C;
        expected[5] = 0x0B0A_0908;
        expected[9] = 0x0706_0504;
        write_guest(
            &mut engine,
            p_bits,
            &texels_to_bytes(&[expected[0], expected[1]]),
        );
        write_guest(
            &mut engine,
            p_bits.saturating_add(16),
            &texels_to_bytes(&[expected[4], expected[5]]),
        );
        write_guest(
            &mut engine,
            p_bits.saturating_add(32),
            &texels_to_bytes(&[expected[8], expected[9]]),
        );

        assert_eq!(
            unlock_rect_render_target(&mut engine, &mut state, RT_VA),
            D3D_OK
        );
        assert_eq!(
            render_target_texels(&mut state),
            expected,
            "every rect row round-trips through the pitched block"
        );
    }

    #[test]
    fn texture_lock_round_trips_a_full_surface() {
        let (mut engine, mut state) = setup();
        let texels = sample_texels(12); // 4x3
        insert_texture(&mut state, 4, 3, texels);

        let result = lock_rect_common(&mut engine, &mut state, TEXTURE_VA, 0, LOCKED_RECT_VA, 0)
            .expect("texture lock runs");
        assert_eq!(result, D3D_OK);

        let (pitch, p_bits) = locked_rect_of(&mut engine);
        assert_eq!(pitch, 16, "pitch is width * 4");
        let written = sample_texels(12)
            .iter()
            .map(|texel| texel ^ 0x00FF_FFFF)
            .collect::<Vec<u32>>();
        write_guest(&mut engine, p_bits, &texels_to_bytes(&written));

        assert_eq!(
            unlock_rect_common(&mut engine, &mut state, TEXTURE_VA),
            D3D_OK
        );
        assert_eq!(
            texture_texels(&mut state),
            written,
            "the guest's texels land back in the texture"
        );
    }

    #[test]
    fn texture_unlock_writes_back_only_the_clipped_rect() {
        let (mut engine, mut state) = setup();
        let texels = sample_texels(12); // 4x3
        insert_texture(&mut state, 4, 3, texels.clone());

        let rect = (1, 1, 3, 2);
        put_rect(&mut engine, rect);
        let result = lock_rect_common(
            &mut engine,
            &mut state,
            TEXTURE_VA,
            0,
            LOCKED_RECT_VA,
            RECT_VA,
        )
        .expect("clipped texture lock runs");
        assert_eq!(result, D3D_OK);

        let (pitch, p_bits) = locked_rect_of(&mut engine);
        assert_eq!(pitch, 16);
        // One row of two texels, written through the rect's pointer.
        let mut expected = texels;
        expected[5] = 0x7777_0001;
        expected[6] = 0x7777_0002;
        write_guest(
            &mut engine,
            p_bits,
            &texels_to_bytes(&[expected[5], expected[6]]),
        );

        assert_eq!(
            unlock_rect_common(&mut engine, &mut state, TEXTURE_VA),
            D3D_OK
        );
        assert_eq!(
            texture_texels(&mut state),
            expected,
            "only the rect's texels change; the rest of the row and surface stay"
        );
    }
}
