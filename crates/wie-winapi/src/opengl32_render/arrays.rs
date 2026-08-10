// ── Vertex arrays (GL 1.1) + buffer objects (GL 1.5) ───────────────────
//
// Client-side arrays: `glVertexPointer`/`glColorPointer`/`glTexCoordPointer`/
// `glNormalPointer` record {size, type, stride, source} per array; the
// enabled arrays are pulled by `glDrawArrays`/`glDrawElements` at draw time
// (arrays without an enabled slot fall back to the CURRENT color/texcoord/
// normal — GL semantics). A pointer is either a GUEST VA (read through the
// injected reader at draw time) or an OFFSET into the buffer object bound to
// `GL_ARRAY_BUFFER` (GL 1.5 VBO semantics). Index lists come from the guest
// or from the `GL_ELEMENT_ARRAY_BUFFER` binding.
//
// The fetched vertices run through the same immediate-mode emission core
// (`draw_gl_vertices`), so arrays share the transform/clip/rasterize path.
// Lighting (per-vertex) applies at fetch time exactly like immediate mode.

use super::lists::{ListOp, gl_capture};
use super::*;

/// `GL_VERTEX_ARRAY` (gl.h).
pub(crate) const GL_VERTEX_ARRAY: u32 = 0x8074;
/// `GL_NORMAL_ARRAY`.
pub(crate) const GL_NORMAL_ARRAY: u32 = 0x8075;
/// `GL_COLOR_ARRAY`.
pub(crate) const GL_COLOR_ARRAY: u32 = 0x8076;
/// `GL_TEXTURE_COORD_ARRAY`.
pub(crate) const GL_TEXTURE_COORD_ARRAY: u32 = 0x8078;
/// `GL_FLOAT` (pointer type).
pub(crate) const GL_FLOAT: u32 = 0x1406;
/// `GL_DOUBLE` (pointer type).
pub(crate) const GL_DOUBLE: u32 = 0x140A;
/// `GL_SHORT` (pointer type).
pub(crate) const GL_SHORT: u32 = 0x1402;
/// `GL_UNSIGNED_SHORT` (index type).
pub(crate) const GL_UNSIGNED_SHORT: u32 = 0x1403;
/// `GL_INT` (index type).
pub(crate) const GL_INT: u32 = 0x1404;
/// `GL_UNSIGNED_INT` (index type).
pub(crate) const GL_UNSIGNED_INT: u32 = 0x1405;
/// `GL_ARRAY_BUFFER`.
pub(crate) const GL_ARRAY_BUFFER: u32 = 0x8892;
/// `GL_ELEMENT_ARRAY_BUFFER`.
pub(crate) const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
/// `GL_ARRAY_BUFFER_BINDING`.
pub(crate) const GL_ARRAY_BUFFER_BINDING: u32 = 0x8894;
/// `GL_ELEMENT_ARRAY_BUFFER_BINDING`.
/// `GL_STATIC_DRAW` (buffer usage — accepted, ignored). The usage hint is
/// not modelled; the constant documents the accepted value (referenced by
/// the unit tests).
#[allow(dead_code)]
pub(crate) const GL_STATIC_DRAW: u32 = 0x88E4;

pub(crate) const GL_ELEMENT_ARRAY_BUFFER_BINDING: u32 = 0x8895;
/// Where a client array's data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArraySource {
    /// A guest VA (client memory), read at draw time.
    Guest(u64),
    /// An offset into the named buffer object (`GL_ARRAY_BUFFER` binding).
    Vbo { id: u32, offset: u64 },
}

/// One client-side vertex array (position / color / texcoord / normal).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientArray {
    /// `glEnableClientState` flag.
    pub(crate) enabled: bool,
    /// Components per vertex (2..4 positions, 3/4 colors, 1..4 texcoords,
    /// 3 normals).
    pub(crate) size: u32,
    /// Component type (`GL_FLOAT` / `GL_DOUBLE` / `GL_SHORT` /
    /// `GL_UNSIGNED_BYTE`).
    pub(crate) type_: u32,
    /// Byte stride between vertices (0 = tightly packed).
    pub(crate) stride: u32,
    /// Data source (guest VA or VBO offset).
    pub(crate) source: ArraySource,
}

impl Default for ClientArray {
    fn default() -> Self {
        Self {
            enabled: false,
            size: 0,
            type_: GL_FLOAT,
            stride: 0,
            source: ArraySource::Guest(0),
        }
    }
}

/// A buffer object (`glGenBuffers`/`glBindBuffer`/`glBufferData`).
#[derive(Debug)]
pub(crate) struct BufferObject {
    /// GL buffer name (never 0).
    pub(crate) id: u32,
    /// Host copy of the buffer bytes (GL copies at `glBufferData` time).
    pub(crate) data: Vec<u8>,
}

/// A guest-memory reader injected by the ABI handler (`engine.mem_read`).
pub(crate) type GuestRead<'a> = &'a mut dyn FnMut(u64, &mut [u8]) -> bool;

/// `glVertexPointer(size, type, stride, pointer)` — the pointer is a guest
/// VA, or a VBO offset when a buffer is bound to `GL_ARRAY_BUFFER`.
pub(crate) fn gl_vertex_pointer(ctx: &mut GlCtx, size: u32, type_: u32, stride: u32, pointer: u64) {
    if !matches!(size, 2..=4) {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    if !is_pointer_type(type_) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.vertex_array.size = size;
    ctx.vertex_array.type_ = type_;
    ctx.vertex_array.stride = stride;
    ctx.vertex_array.source = resolve_pointer_source(ctx, pointer);
}

/// `glColorPointer(size, type, stride, pointer)`.
pub(crate) fn gl_color_pointer(ctx: &mut GlCtx, size: u32, type_: u32, stride: u32, pointer: u64) {
    if !matches!(size, 3 | 4) {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    if !matches!(type_, GL_FLOAT | GL_UNSIGNED_BYTE) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.color_array.size = size;
    ctx.color_array.type_ = type_;
    ctx.color_array.stride = stride;
    ctx.color_array.source = resolve_pointer_source(ctx, pointer);
}

/// `glTexCoordPointer(size, type, stride, pointer)`.
pub(crate) fn gl_texcoord_pointer(
    ctx: &mut GlCtx,
    size: u32,
    type_: u32,
    stride: u32,
    pointer: u64,
) {
    if !matches!(size, 1..=4) {
        ctx.set_error(GL_INVALID_VALUE);
        return;
    }
    if !is_pointer_type(type_) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.texcoord_array.size = size;
    ctx.texcoord_array.type_ = type_;
    ctx.texcoord_array.stride = stride;
    ctx.texcoord_array.source = resolve_pointer_source(ctx, pointer);
}

/// `glNormalPointer(type, stride, pointer)`.
pub(crate) fn gl_normal_pointer(ctx: &mut GlCtx, type_: u32, stride: u32, pointer: u64) {
    if !is_pointer_type(type_) {
        ctx.set_error(GL_INVALID_ENUM);
        return;
    }
    ctx.normal_array.size = 3;
    ctx.normal_array.type_ = type_;
    ctx.normal_array.stride = stride;
    ctx.normal_array.source = resolve_pointer_source(ctx, pointer);
}

/// A pointer argument is a VBO offset when a buffer is bound to
/// `GL_ARRAY_BUFFER`, else a guest VA.
#[must_use]
fn resolve_pointer_source(ctx: &GlCtx, pointer: u64) -> ArraySource {
    if ctx.bound_array_buffer != 0 {
        ArraySource::Vbo {
            id: ctx.bound_array_buffer,
            offset: pointer,
        }
    } else {
        ArraySource::Guest(pointer)
    }
}

#[must_use]
fn is_pointer_type(type_: u32) -> bool {
    matches!(type_, GL_FLOAT | GL_DOUBLE | GL_SHORT)
}

/// `glEnableClientState` / `glDisableClientState`.
pub(crate) fn client_state(ctx: &mut GlCtx, array: u32, enabled: bool) {
    match array {
        GL_VERTEX_ARRAY => ctx.vertex_array.enabled = enabled,
        GL_COLOR_ARRAY => ctx.color_array.enabled = enabled,
        GL_TEXTURE_COORD_ARRAY => ctx.texcoord_array.enabled = enabled,
        GL_NORMAL_ARRAY => ctx.normal_array.enabled = enabled,
        _ => ctx.set_error(GL_INVALID_ENUM),
    }
}

/// Bytes per component for a pointer/index type (0 = unsupported).
#[must_use]
fn component_bytes(type_: u32) -> u32 {
    match type_ {
        GL_FLOAT | GL_INT | GL_UNSIGNED_INT => 4,
        GL_DOUBLE => 8,
        GL_SHORT | GL_UNSIGNED_SHORT => 2,
        GL_UNSIGNED_BYTE => 1,
        _ => 0,
    }
}

/// Fetch one vertex's components from a client array; `None` when the array
/// is disabled or its backing memory cannot be read (the vertex is dropped).
fn fetch_components(
    arr: &ClientArray,
    index: u32,
    read: GuestRead<'_>,
    buffers: &[BufferObject],
) -> Option<Vec<f32>> {
    if !arr.enabled || arr.size == 0 {
        return None;
    }
    let type_size = component_bytes(arr.type_);
    if type_size == 0 {
        return None;
    }
    let comps = usize::try_from(arr.size).unwrap_or(0);
    let effective_stride = if arr.stride == 0 {
        arr.size.saturating_mul(type_size)
    } else {
        arr.stride
    };
    let row_bytes = comps.saturating_mul(usize::try_from(type_size).unwrap_or(0));
    let row_offset = u64::from(index).saturating_mul(u64::from(effective_stride));
    let mut row = vec![0_u8; row_bytes];
    let ok = match arr.source {
        ArraySource::Guest(va) => read(va.saturating_add(row_offset), &mut row),
        ArraySource::Vbo { id, offset } => {
            let bytes = buffer_bytes(buffers, id)?;
            let start = usize::try_from(offset.saturating_add(row_offset)).unwrap_or(usize::MAX);
            let end = start.saturating_add(row_bytes);
            let slice = bytes.get(start..end)?;
            row.copy_from_slice(slice);
            true
        }
    };
    if !ok {
        return None;
    }
    let mut out = Vec::with_capacity(comps);
    for c in 0..comps {
        let off = c.saturating_mul(usize::try_from(type_size).unwrap_or(0));
        out.push(parse_component(&row, off, arr.type_)?);
    }
    Some(out)
}

/// Parse one component at `off` into `f32` (byte colors normalize to 0..1).
#[must_use]
fn parse_component(row: &[u8], off: usize, type_: u32) -> Option<f32> {
    let end = off.saturating_add(4);
    match type_ {
        GL_FLOAT => row
            .get(off..end)
            .and_then(|s| s.try_into().ok())
            .map(f32::from_le_bytes),
        GL_DOUBLE => row
            .get(off..off.saturating_add(8))
            .and_then(|s| s.try_into().ok())
            .map(f64::from_le_bytes)
            .map(|v| v as f32),
        GL_SHORT => row
            .get(off..off.saturating_add(2))
            .and_then(|s| s.try_into().ok())
            .map(i16::from_le_bytes)
            .map(f32::from),
        GL_UNSIGNED_BYTE => row.get(off).copied().map(|v| f32::from(v) / 255.0),
        _ => None,
    }
}

/// The host bytes of a buffer object, if it exists.
#[must_use]
fn buffer_bytes(buffers: &[BufferObject], id: u32) -> Option<&[u8]> {
    buffers
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.data.as_slice())
}

/// Build one array-draw vertex (position + current/array color/texcoord +
/// normal, lit when `GL_LIGHTING` is on).
fn array_vertex(
    ctx: &GlCtx,
    index: u32,
    read: GuestRead<'_>,
    buffers: &[BufferObject],
) -> Option<GlVertex> {
    let pos = fetch_components(&ctx.vertex_array, index, read, buffers)?;
    let x = pos.first().copied().unwrap_or(0.0);
    let y = pos.get(1).copied().unwrap_or(0.0);
    let z = pos.get(2).copied().unwrap_or(0.0);
    let w = pos.get(3).copied().unwrap_or(1.0);
    let color = if ctx.color_array.enabled {
        fetch_components(&ctx.color_array, index, read, buffers).map_or(ctx.current_color, |c| {
            [
                c.first().copied().unwrap_or(1.0),
                c.get(1).copied().unwrap_or(1.0),
                c.get(2).copied().unwrap_or(1.0),
                c.get(3).copied().unwrap_or(1.0),
            ]
        })
    } else {
        ctx.current_color
    };
    let tex = if ctx.texcoord_array.enabled {
        fetch_components(&ctx.texcoord_array, index, read, buffers).map_or(
            ctx.current_texcoord,
            |c| {
                [
                    c.first().copied().unwrap_or(0.0),
                    c.get(1).copied().unwrap_or(0.0),
                    c.get(2).copied().unwrap_or(0.0),
                    c.get(3).copied().unwrap_or(1.0),
                ]
            },
        )
    } else {
        ctx.current_texcoord
    };
    let normal = if ctx.normal_array.enabled {
        fetch_components(&ctx.normal_array, index, read, buffers).map_or(ctx.current_normal, |c| {
            [
                c.first().copied().unwrap_or(0.0),
                c.get(1).copied().unwrap_or(0.0),
                c.get(2).copied().unwrap_or(0.0),
            ]
        })
    } else {
        ctx.current_normal
    };
    let color = ctx.lit_color([x, y, z, w], normal, color);
    Some(GlVertex {
        pos: [x, y, z, w],
        color,
        tex,
        normal,
    })
}

/// `glDrawArrays(mode, first, count)` — pull `count` vertices from the
/// enabled arrays and emit `mode`.
pub(crate) fn gl_draw_arrays(
    ctx: &mut GlCtx,
    mode: u32,
    first: u32,
    count: u32,
    read: GuestRead<'_>,
) {
    if !gl_capture(ctx, ListOp::DrawArrays { mode, first, count }) {
        return;
    }
    if ctx.begin_mode.is_some() {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    let mut batch = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for i in 0..count {
        let index = first.saturating_add(i);
        if let Some(v) = array_vertex(ctx, index, &mut *read, &ctx.buffers) {
            batch.push(v);
        }
    }
    draw_gl_vertices(ctx, mode, &batch);
}

/// `glDrawElements(mode, count, type, indices)` — indices from the element
/// buffer (offset) or guest memory, each selecting one array vertex.
pub(crate) fn gl_draw_elements(
    ctx: &mut GlCtx,
    mode: u32,
    count: u32,
    index_type: u32,
    indices_va: u64,
    read: GuestRead<'_>,
) {
    if !gl_capture(
        ctx,
        ListOp::DrawElements {
            mode,
            count,
            index_type,
            indices_va,
        },
    ) {
        return;
    }
    if ctx.begin_mode.is_some() {
        ctx.set_error(GL_INVALID_OPERATION);
        return;
    }
    let width = match index_type {
        GL_UNSIGNED_BYTE => 1,
        GL_UNSIGNED_SHORT => 2,
        GL_INT | GL_UNSIGNED_INT => 4,
        _ => {
            ctx.set_error(GL_INVALID_ENUM);
            return;
        }
    };
    let count_us = usize::try_from(count).unwrap_or(0);
    let mut indices = Vec::with_capacity(count_us);
    if ctx.bound_element_buffer != 0 {
        let Some(bytes) = buffer_bytes(&ctx.buffers, ctx.bound_element_buffer) else {
            return;
        };
        for j in 0..count_us {
            let off = usize::try_from(indices_va.saturating_add(u64::try_from(j).unwrap_or(0)))
                .unwrap_or(usize::MAX)
                .saturating_mul(usize::try_from(width).unwrap_or(0));
            let value = match width {
                1 => u32::from(bytes.get(off).copied().unwrap_or(0)),
                2 => u32::from(u16::from_le_bytes(
                    bytes
                        .get(off..off.saturating_add(2))
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 2]),
                )),
                _ => u32::from_le_bytes(
                    bytes
                        .get(off..off.saturating_add(4))
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 4]),
                ),
            };
            indices.push(value);
        }
    } else {
        let byte_len = count_us.saturating_mul(usize::try_from(width).unwrap_or(0));
        let mut bytes = vec![0_u8; byte_len];
        if !read(indices_va, &mut bytes) {
            return;
        }
        for j in 0..count_us {
            let off = j.saturating_mul(usize::try_from(width).unwrap_or(0));
            let value = match width {
                1 => u32::from(bytes.get(off).copied().unwrap_or(0)),
                2 => u32::from(u16::from_le_bytes(
                    bytes
                        .get(off..off.saturating_add(2))
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 2]),
                )),
                _ => u32::from_le_bytes(
                    bytes
                        .get(off..off.saturating_add(4))
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 4]),
                ),
            };
            indices.push(value);
        }
    }
    let mut batch = Vec::with_capacity(count_us);
    for index in indices {
        if let Some(v) = array_vertex(ctx, index, &mut *read, &ctx.buffers) {
            batch.push(v);
        }
    }
    draw_gl_vertices(ctx, mode, &batch);
}

// ── Buffer objects ──────────────────────────────────────────────────────

/// `glGenBuffers(n, buffers)` — allocate `n` names (objects form on bind).
pub(crate) fn gl_gen_buffers(ctx: &mut GlCtx, count: u32) -> Vec<u32> {
    let mut names = Vec::new();
    for _ in 0..count {
        let mut name = ctx.next_buffer_id;
        while name == 0 || ctx.buffers.iter().any(|b| b.id == name) {
            name = name.wrapping_add(1);
        }
        ctx.next_buffer_id = name.wrapping_add(1);
        names.push(name);
    }
    names
}

/// `glDeleteBuffers(n, buffers)` — remove the objects; unbind deleted ones.
pub(crate) fn gl_delete_buffers(ctx: &mut GlCtx, names: &[u32]) {
    for name in names {
        if *name == ctx.bound_array_buffer {
            ctx.bound_array_buffer = 0;
        }
        if *name == ctx.bound_element_buffer {
            ctx.bound_element_buffer = 0;
        }
        ctx.buffers.retain(|b| b.id != *name);
    }
}

/// `glIsBuffer(id)`.
#[must_use]
pub(crate) fn gl_is_buffer(ctx: &GlCtx, id: u32) -> bool {
    ctx.buffers.iter().any(|b| b.id == id)
}

/// `glBindBuffer(target, id)` — create the object on first bind (GL rule).
pub(crate) fn gl_bind_buffer(ctx: &mut GlCtx, target: u32, id: u32) {
    let binding = match target {
        GL_ARRAY_BUFFER => &mut ctx.bound_array_buffer,
        GL_ELEMENT_ARRAY_BUFFER => &mut ctx.bound_element_buffer,
        _ => {
            ctx.set_error(GL_INVALID_ENUM);
            return;
        }
    };
    if id == 0 {
        *binding = 0;
        return;
    }
    if !ctx.buffers.iter().any(|b| b.id == id) {
        ctx.buffers.push(BufferObject {
            id,
            data: Vec::new(),
        });
    }
    *binding = id;
}

/// `glBufferData(target, size, data, usage)` — copy `size` guest bytes into
/// the bound buffer (NULL data → undefined, zeroed here).
pub(crate) fn gl_buffer_data(
    ctx: &mut GlCtx,
    target: u32,
    size: u32,
    data_va: u64,
    _usage: u32,
    read: GuestRead<'_>,
) {
    let id = buffer_binding(ctx, target);
    let Some(buffer) = ctx.buffers.iter_mut().find(|b| b.id == id) else {
        return; // no bound buffer: GL ignores the call
    };
    let len = usize::try_from(size).unwrap_or(0);
    let mut data = vec![0_u8; len];
    if data_va != 0 {
        let _ = read(data_va, &mut data); // unreadable → zeroed (undefined)
    }
    buffer.data = data;
}

/// `glBufferSubData(target, offset, size, data)` — partial overwrite.
pub(crate) fn gl_buffer_sub_data(
    ctx: &mut GlCtx,
    target: u32,
    offset: u32,
    size: u32,
    data_va: u64,
    read: GuestRead<'_>,
) {
    let id = buffer_binding(ctx, target);
    let Some(buffer) = ctx.buffers.iter_mut().find(|b| b.id == id) else {
        return;
    };
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let len = usize::try_from(size).unwrap_or(0);
    let end = start.saturating_add(len);
    if end > buffer.data.len() {
        return; // out of range: GL would raise an error; skip defensively
    }
    let mut data = vec![0_u8; len];
    if data_va != 0 {
        let _ = read(data_va, &mut data);
    }
    if let Some(slot) = buffer.data.get_mut(start..end) {
        slot.copy_from_slice(&data);
    }
}

/// The currently bound buffer id for `target` (0 = none).
#[must_use]
fn buffer_binding(ctx: &GlCtx, target: u32) -> u32 {
    match target {
        GL_ARRAY_BUFFER => ctx.bound_array_buffer,
        GL_ELEMENT_ARRAY_BUFFER => ctx.bound_element_buffer,
        _ => 0,
    }
}

/// `(GL_ARRAY_BUFFER_BINDING, GL_ELEMENT_ARRAY_BUFFER_BINDING)`.
#[must_use]
pub(crate) fn buffer_bindings(ctx: &GlCtx) -> (u32, u32) {
    (ctx.bound_array_buffer, ctx.bound_element_buffer)
}
