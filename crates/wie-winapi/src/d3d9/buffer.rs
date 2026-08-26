//! `IDirect3DVertexBuffer9` / `IDirect3DIndexBuffer9` COM objects.
//!
//! L2 (ora-5's design): the Create*Buffer device handlers become real and
//! hand out these objects. Each buffer is a host-owned byte store (the
//! texture-record pattern): `Lock` hands the guest a coherent block (the
//! whole buffer, `ppbData` = block + OffsetToLock), the guest writes through
//! normal memory writes, and `Unlock` copies the block back into the host
//! record. The buffer-form draws read the host copy, so a draw after Unlock
//! never touches guest memory.
//!
//! Pool semantics: D3DPOOL_DEFAULT/MANAGED/SYSTEMMEM all resolve to the same
//! host store — there is no GPU memory in a software device, so the pool only
//! changes the honest GetDesc value. D3DPOOL_SCRATCH is rejected at Create
//! (real D3D9 forbids scratch vertex/index buffers). The D3DLOCK_DISCARD /
//! D3DLOCK_NOOVERWRITE hints are accepted and ignored: they exist to let the
//! GPU overlap uploads with reads, a race the host-side copy-back cannot
//! have.

use crate::gdi32::{ArgReg, read_arg};
use anyhow::{Context, Result};

use super::{
    D3D_OK, D3DERR_INVALIDCALL, IDIRECT3DINDEXBUFFER9_ALLOCATION_SIZE,
    IDIRECT3DINDEXBUFFER9_METHOD_COUNT, IDIRECT3DINDEXBUFFER9_OBJECT_OFFSET,
    IDIRECT3DVERTEXBUFFER9_ALLOCATION_SIZE, IDIRECT3DVERTEXBUFFER9_METHOD_COUNT,
    IDIRECT3DVERTEXBUFFER9_OBJECT_OFFSET, allocate_direct3d_block,
};
use crate::fake_va::D3d9Iface;
use crate::guest_memory::write_u64 as write_guest_u64;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::texture::fill_com_vtable;
use crate::kernel32::low_u32;

/// `E_NOINTERFACE` — QueryInterface for an IID this object does not expose.
const E_NOINTERFACE: u64 = 0x8000_4002;

/// `IID_IUnknown` ({00000000-0000-0000-C000-000000000046}) in memory order.
const IID_IUNKNOWN: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];
/// `IID_IDirect3DVertexBuffer9` ({B64BB1B5-FD70-4DF6-BF91-19D0A1249EFC}) in
/// memory order. `Data1` renders as `B64BB1B5`, whose little-endian image is
/// `B5 B1 4B B6` — the second byte was previously mistyped as `0x1B`.
const IID_IDIRECT3DVERTEXBUFFER9: [u8; 16] = [
    0xB5, 0xB1, 0x4B, 0xB6, 0x70, 0xFD, 0xF6, 0x4D, 0xBF, 0x91, 0x19, 0xD0, 0xA1, 0x24, 0x9E, 0xFC,
];
/// `IID_IDirect3DIndexBuffer9` ({7C9DD65E-D3F7-4529-ACEE-78530F31DE25}).
const IID_IDIRECT3DINDEXBUFFER9: [u8; 16] = [
    0x5E, 0xD6, 0x9D, 0x7C, 0xF7, 0xD3, 0x29, 0x45, 0xAC, 0xEE, 0x78, 0x53, 0x0F, 0x31, 0xDE, 0x25,
];

/// `D3DRTYPE_VERTEXBUFFER` — the D3DRESOURCETYPE a vertex buffer reports.
const D3DRTYPE_VERTEXBUFFER: u32 = 6;
/// `D3DRTYPE_INDEXBUFFER`.
const D3DRTYPE_INDEXBUFFER: u32 = 7;

/// `D3DPOOL_SCRATCH` — only textures may use it (rejected at Create).
const D3DPOOL_SCRATCH: u32 = 3;

/// Which stream a buffer record feeds (drives GetDesc and the draw gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    /// `CreateVertexBuffer` — the FVF from Create (reported by GetDesc).
    Vertex { fvf: u32 },
    /// `CreateIndexBuffer` — the index format (D3DFMT_INDEX16/32).
    Index { format: u32 },
}

/// A D3D9 vertex/index buffer: host-owned bytes plus the guest lock state.
///
/// The record's guest VA is also the COM object pointer handed to the guest.
#[derive(Debug, Clone)]
pub struct BufferRecord {
    /// The buffer object's guest VA (also the `IDirect3D*Buffer9` pointer).
    pub handle: u64,
    /// Vertex or index buffer (the format/FVF from Create).
    pub kind: BufferKind,
    /// Buffer size in bytes (`Length` from Create).
    pub size: u32,
    /// `Usage` flags from Create (D3DUSAGE_*; informational in a software
    /// device).
    pub usage: u32,
    /// `Pool` from Create (D3DPOOL_DEFAULT/MANAGED/SYSTEMMEM).
    pub pool: u32,
    /// Host-owned bytes (the authoritative copy after `Unlock`).
    pub data: Vec<u8>,
    /// Guest block VA handed out by the active `Lock` (0 = not locked).
    pub locked_va: u64,
    /// COM reference count (AddRef/Release; freed at 0).
    pub ref_count: u32,
}

/// Read a guest IID (16 bytes) at `iid_va`.
fn read_guest_iid(engine: &mut dyn wie_cpu::CpuEngine, iid_va: u64) -> Option<[u8; 16]> {
    if iid_va == 0 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    engine.mem_read(iid_va, &mut bytes).ok()?;
    Some(bytes)
}

/// Allocate a buffer object (vtable + COM object) and return its pointer.
fn allocate_buffer_object(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    iface: D3d9Iface,
) -> Result<u64> {
    let (size, count, object_offset) = match iface {
        D3d9Iface::VertexBuffer9 => (
            IDIRECT3DVERTEXBUFFER9_ALLOCATION_SIZE,
            IDIRECT3DVERTEXBUFFER9_METHOD_COUNT,
            IDIRECT3DVERTEXBUFFER9_OBJECT_OFFSET,
        ),
        _ => (
            IDIRECT3DINDEXBUFFER9_ALLOCATION_SIZE,
            IDIRECT3DINDEXBUFFER9_METHOD_COUNT,
            IDIRECT3DINDEXBUFFER9_OBJECT_OFFSET,
        ),
    };
    let vtable_address = allocate_direct3d_block(engine, state, size, "IDirect3DBuffer9");
    if vtable_address == 0 {
        return Ok(0);
    }
    fill_com_vtable(engine, vtable_address, iface, count)?;
    let object_address = vtable_address
        .checked_add(object_offset)
        .context("IDirect3DBuffer9 object address overflow")?;
    write_guest_u64(engine, object_address, vtable_address)
        .context("failed to initialize IDirect3DBuffer9 object")?;
    Ok(object_address)
}

/// The IID this buffer object answers `QueryInterface` with (its own
/// interface or `IID_IUnknown`).
fn buffer_self_iid(record: &BufferRecord) -> [u8; 16] {
    match record.kind {
        BufferKind::Vertex { .. } => IID_IDIRECT3DVERTEXBUFFER9,
        BufferKind::Index { .. } => IID_IDIRECT3DINDEXBUFFER9,
    }
}

/// Shared QueryInterface body: the object answers for `IID_IUnknown` and its
/// own interface; everything else is `E_NOINTERFACE` with `*ppvObject = NULL`.
fn buffer_query_interface_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    this_pointer: u64,
) -> Result<u64> {
    let iid_va = read_arg(engine, ArgReg::Rdx, "QueryInterface")?;
    let ppv_object = read_arg(engine, ArgReg::R8, "QueryInterface")?;

    let known_buffer = state.d3d9().d3d9_buffers.contains_key(&this_pointer);
    if !known_buffer {
        if ppv_object != 0 {
            write_guest_u64(engine, ppv_object, 0).context("failed to clear QI output")?;
        }
        return Ok(E_NOINTERFACE);
    }
    let requested = read_guest_iid(engine, iid_va).unwrap_or([0; 16]);
    // The self IID depends only on the record kind, which the object type
    // pins at Create time.
    let self_iid = state
        .d3d9()
        .d3d9_buffers
        .get(&this_pointer)
        .map(buffer_self_iid);
    let supported = self_iid.is_some_and(|iid| requested == IID_IUNKNOWN || requested == iid);
    let return_value = if supported {
        // Real COM AddRefs on a successful QI.
        if let Some(record) = state.d3d9().d3d9_buffers.get_mut(&this_pointer) {
            record.ref_count = record.ref_count.saturating_add(1);
        }
        if ppv_object != 0 {
            write_guest_u64(engine, ppv_object, this_pointer)
                .context("failed to write QI output")?;
        }
        D3D_OK
    } else {
        if ppv_object != 0 {
            write_guest_u64(engine, ppv_object, 0).context("failed to clear QI output")?;
        }
        E_NOINTERFACE
    };
    Ok(return_value)
}

/// Shared AddRef body: bump the record's ref count and return it.
fn buffer_add_ref_common(state: &mut WinApiState, this_pointer: u64) -> u64 {
    let Some(record) = state.d3d9().d3d9_buffers.get_mut(&this_pointer) else {
        return 0;
    };
    record.ref_count = record.ref_count.saturating_add(1);
    u64::from(record.ref_count)
}

/// Shared Release body: drop the ref count; at zero, free the vtable block
/// and the lock block (if any) and unbind the buffer from the stream/indices.
fn buffer_release_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    this_pointer: u64,
) -> Result<u64> {
    let remaining = {
        let record = state.d3d9().d3d9_buffers.get_mut(&this_pointer);
        let Some(record) = record else {
            return Ok(0);
        };
        record.ref_count = record.ref_count.saturating_sub(1);
        record.ref_count
    };
    if remaining == 0 {
        let locked_va = state
            .d3d9()
            .d3d9_buffers
            .get(&this_pointer)
            .map_or(0, |record| record.locked_va);
        let object_offset = match state.d3d9().d3d9_buffers.get(&this_pointer) {
            Some(record) => match record.kind {
                BufferKind::Vertex { .. } => IDIRECT3DVERTEXBUFFER9_OBJECT_OFFSET,
                BufferKind::Index { .. } => IDIRECT3DINDEXBUFFER9_OBJECT_OFFSET,
            },
            None => 0,
        };
        if locked_va != 0 {
            let _ = state
                .heap_state
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, locked_va);
        }
        // Unbind from the device state (a released buffer must not draw).
        let d3d = state.d3d9();
        if d3d.d3d9_stream_source_va == this_pointer {
            d3d.d3d9_stream_source_va = 0;
        }
        if d3d.d3d9_index_buffer_va == this_pointer {
            d3d.d3d9_index_buffer_va = 0;
        }
        state.d3d9().d3d9_buffers.remove(&this_pointer);
        let vtable = this_pointer
            .checked_sub(object_offset)
            .context("IDirect3DBuffer9 allocation address underflow")?;
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, vtable);
    }
    Ok(u64::from(remaining))
}

/// Read the Lock arguments shared by both buffer types.
struct LockArgs {
    offset: u32,
    size: u32,
    ppb_data: u64,
}

fn read_lock_args(engine: &mut dyn wie_cpu::CpuEngine, method_name: &str) -> Result<LockArgs> {
    let offset_raw = read_arg(engine, ArgReg::Rdx, method_name)?;
    let size_raw = read_arg(engine, ArgReg::R8, method_name)?;
    let ppb_data = read_arg(engine, ArgReg::R9, method_name)?;
    Ok(LockArgs {
        offset: low_u32(offset_raw, "Lock offset")?,
        size: low_u32(size_raw, "Lock size")?,
        ppb_data,
    })
}

/// Shared Lock body: hand the guest a coherent block covering the whole
/// buffer and remember it for the copy-back. `SizeToLock == 0` locks the rest
/// of the buffer from `OffsetToLock` (real D3D9 semantics).
fn buffer_lock_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    this_pointer: u64,
) -> Result<u64> {
    let args = read_lock_args(engine, "IDirect3DBuffer9::Lock")?;
    let Some(record) = state.d3d9().d3d9_buffers.get(&this_pointer) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    if record.locked_va != 0 || args.ppb_data == 0 {
        return Ok(D3DERR_INVALIDCALL); // double lock / null output
    }
    let size = record.size;
    let lock_size = if args.size == 0 {
        size.saturating_sub(args.offset)
    } else {
        args.size
    };
    // The locked range must lie inside the buffer (offset + size ≤ size).
    if args.offset > size || lock_size > size.saturating_sub(args.offset) {
        return Ok(D3DERR_INVALIDCALL);
    }

    let block = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, u64::from(size));
    if block == 0 {
        return Ok(D3DERR_INVALIDCALL); // allocation failed
    }
    let p_data = block.saturating_add(u64::from(args.offset));
    write_guest_u64(engine, args.ppb_data, p_data)
        .context("failed to write Lock output pointer")?;
    if let Some(record) = state.d3d9().d3d9_buffers.get_mut(&this_pointer) {
        record.locked_va = block;
    }
    Ok(D3D_OK)
}

/// Shared Unlock body: copy the whole lock block back into the host record
/// and free the guest block.
fn buffer_unlock_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    this_pointer: u64,
) -> Result<u64> {
    let Some(record) = state.d3d9().d3d9_buffers.get(&this_pointer) else {
        return Ok(D3DERR_INVALIDCALL);
    };
    let locked_va = record.locked_va;
    let size = usize::try_from(record.size).unwrap_or(0);
    if locked_va == 0 {
        return Ok(D3DERR_INVALIDCALL); // not locked
    }
    let mut bytes = vec![0_u8; size];
    if engine.mem_read(locked_va, &mut bytes).is_ok()
        && let Some(record) = state.d3d9().d3d9_buffers.get_mut(&this_pointer)
        && record.data.len() == bytes.len()
    {
        record.data.copy_from_slice(&bytes);
    }
    let _ = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .free_coherent(engine, locked_va);
    if let Some(record) = state.d3d9().d3d9_buffers.get_mut(&this_pointer) {
        record.locked_va = 0;
    }
    Ok(D3D_OK)
}

// ── IDirect3DVertexBuffer9 handlers (vtable slots 0, 1, 2, 11, 12, 13) ──

/// Handles `IDirect3DVertexBuffer9::QueryInterface` (slot 0).
pub fn handle_vertex_buffer_query_interface(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(
        engine,
        ArgReg::Rcx,
        "IDirect3DVertexBuffer9::QueryInterface",
    )?;
    let return_value = buffer_query_interface_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexBuffer9::AddRef` (slot 1).
pub fn handle_vertex_buffer_add_ref(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexBuffer9::AddRef")?;
    let return_value = buffer_add_ref_common(state, this_pointer);
    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexBuffer9::Release` (slot 2).
pub fn handle_vertex_buffer_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexBuffer9::Release")?;
    let return_value = buffer_release_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexBuffer9::Lock` (slot 11).
pub fn handle_vertex_buffer_lock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexBuffer9::Lock")?;
    let return_value = buffer_lock_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexBuffer9::Unlock` (slot 12).
pub fn handle_vertex_buffer_unlock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexBuffer9::Unlock")?;
    let return_value = buffer_unlock_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexBuffer9::GetDesc` (slot 13).
///
/// Writes a `D3DVERTEXBUFFER_DESC` (24 bytes, offsets verified against
/// d3d9types.h): `Format` (0 — not applicable), `Type` (D3DRTYPE_VERTEXBUFFER),
/// `Usage`, `Pool`, `Size`, `FVF`.
pub fn handle_vertex_buffer_get_desc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexBuffer9::GetDesc")?;
    let desc_va = read_arg(engine, ArgReg::Rdx, "IDirect3DVertexBuffer9::GetDesc")?;

    let return_value = if desc_va != 0 {
        match state.d3d9().d3d9_buffers.get(&this_pointer) {
            Some(record) if matches!(record.kind, BufferKind::Vertex { .. }) => {
                let mut bytes = [0_u8; 24];
                // D3DVERTEXBUFFER_DESC { Format @0, Type @4, Usage @8,
                // Pool @12, Size @16, FVF @20 }.
                let kind = record.kind;
                let fvf = match kind {
                    BufferKind::Vertex { fvf } => fvf,
                    BufferKind::Index { .. } => 0,
                };
                bytes[4..8].copy_from_slice(&D3DRTYPE_VERTEXBUFFER.to_le_bytes());
                bytes[8..12].copy_from_slice(&record.usage.to_le_bytes());
                bytes[12..16].copy_from_slice(&record.pool.to_le_bytes());
                bytes[16..20].copy_from_slice(&record.size.to_le_bytes());
                bytes[20..24].copy_from_slice(&fvf.to_le_bytes());
                engine
                    .mem_write(desc_va, &bytes)
                    .context("failed to write D3DVERTEXBUFFER_DESC")?;
                D3D_OK
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

// ── IDirect3DIndexBuffer9 handlers (vtable slots 0, 1, 2, 11, 12, 13) ──

/// Handles `IDirect3DIndexBuffer9::QueryInterface` (slot 0).
pub fn handle_index_buffer_query_interface(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::QueryInterface")?;
    let return_value = buffer_query_interface_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DIndexBuffer9::AddRef` (slot 1).
pub fn handle_index_buffer_add_ref(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::AddRef")?;
    let return_value = buffer_add_ref_common(state, this_pointer);
    ctx.finish(return_value)
}

/// Handles `IDirect3DIndexBuffer9::Release` (slot 2).
pub fn handle_index_buffer_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::Release")?;
    let return_value = buffer_release_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DIndexBuffer9::Lock` (slot 11).
pub fn handle_index_buffer_lock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::Lock")?;
    let return_value = buffer_lock_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DIndexBuffer9::Unlock` (slot 12).
pub fn handle_index_buffer_unlock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::Unlock")?;
    let return_value = buffer_unlock_common(engine, state, this_pointer)?;
    ctx.finish(return_value)
}

/// Handles `IDirect3DIndexBuffer9::GetDesc` (slot 13).
///
/// Writes a `D3DINDEXBUFFER_DESC` (20 bytes, offsets verified against
/// d3d9types.h): `Format` (the D3DFMT_INDEX16/32 from Create), `Type`
/// (D3DRTYPE_INDEXBUFFER), `Usage`, `Pool`, `Size`.
pub fn handle_index_buffer_get_desc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DIndexBuffer9::GetDesc")?;
    let desc_va = read_arg(engine, ArgReg::Rdx, "IDirect3DIndexBuffer9::GetDesc")?;

    let return_value = if desc_va != 0 {
        match state.d3d9().d3d9_buffers.get(&this_pointer) {
            Some(record) if matches!(record.kind, BufferKind::Index { .. }) => {
                let mut bytes = [0_u8; 20];
                // D3DINDEXBUFFER_DESC { Format @0, Type @4, Usage @8,
                // Pool @12, Size @16 }.
                let kind = record.kind;
                let format = match kind {
                    BufferKind::Index { format } => format,
                    BufferKind::Vertex { .. } => 0,
                };
                bytes[0..4].copy_from_slice(&format.to_le_bytes());
                bytes[4..8].copy_from_slice(&D3DRTYPE_INDEXBUFFER.to_le_bytes());
                bytes[8..12].copy_from_slice(&record.usage.to_le_bytes());
                bytes[12..16].copy_from_slice(&record.pool.to_le_bytes());
                bytes[16..20].copy_from_slice(&record.size.to_le_bytes());
                engine
                    .mem_write(desc_va, &bytes)
                    .context("failed to write D3DINDEXBUFFER_DESC")?;
                D3D_OK
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

// ── Create*Buffer record helpers (used by device.rs) ────────────────────

/// Shared Create*Buffer body: validate, allocate the COM object, and insert
/// the record. `fvf_or_format` is the FVF (vertex) or D3DFMT_INDEX* (index).
pub(crate) fn create_buffer_record(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    iface: D3d9Iface,
    length: u32,
    usage: u32,
    pool: u32,
    fvf_or_format: u32,
) -> Result<u64> {
    let valid_pool = pool != D3DPOOL_SCRATCH;
    if length == 0 || !valid_pool {
        return Ok(0);
    }
    let object = allocate_buffer_object(engine, state, iface)?;
    if object == 0 {
        return Ok(0);
    }
    let kind = match iface {
        D3d9Iface::VertexBuffer9 => BufferKind::Vertex { fvf: fvf_or_format },
        _ => BufferKind::Index {
            format: fvf_or_format,
        },
    };
    let size = usize::try_from(length).unwrap_or(0);
    state.d3d9().d3d9_buffers.insert(
        object,
        BufferRecord {
            handle: object,
            kind,
            size: length,
            usage,
            pool,
            data: vec![0_u8; size],
            locked_va: 0,
            ref_count: 1,
        },
    );
    Ok(object)
}
