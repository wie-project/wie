//! Wave 2 slice 2: the D3D9 draw-command capture stream (the command-buffer
//! half of the render-thread pipeline).
//!
//! With the capture pipeline enabled (`PresentChannel::capture_enabled` — the
//! GUI host spawns it via `GuestHandle::enable_capture_stream`), the D3D9
//! `Draw*` and `Clear` handlers stop rasterizing on the emu thread. They
//! append a self-contained op to the per-device stream
//! (`D3D9State::d3d9_capture_stream`); `Present` flushes the stream to the
//! render thread (`present/stream.rs`), which replays the ops into its own
//! backbuffer and publishes the frame exactly like the commit path does.
//!
//! # Design decisions (the three hard problems from the handoff, resolved)
//!
//! 1. **Textures/shaders are snapshotted at record time** (v1 copies — the
//!    opt-in gate makes the cost acceptable during bring-up; wrapping the
//!    record storage in `Arc` is the follow-up optimization). A draw op
//!    carries clones of the bound stages' texels and the parsed shader
//!    programs, so the render thread never touches `D3D9State`.
//! 2. **Render targets + depth buffers round-trip through the render
//!    thread.** Their texel storage is an `Arc` (see `RenderTargetRecord`).
//!    At each flush the emu thread sends an `Arc` clone of a target's current
//!    texels when the render thread has never seen it (`d3d9_capture_*_seen`
//!    sets) or the emu thread mutated it since (`d3d9_capture_emu_dirty_rt` —
//!    the only emu-side mutation is the `UnlockRect` copy-back). The render
//!    thread replays the ops into its OWN copies and hands its post-frame
//!    buffers back; the emu thread installs the handback at the next flush
//!    boundary AND at every render-target `LockRect`/`UnlockRect` (a sync
//!    point, so guest writes always land on top of the render thread's latest
//!    state). A guest that locks an RT for READ-BACK while ops are still
//!    pending triggers `sync_flush_for_target_read`: the pending stream is
//!    flushed immediately (zero publish dims — no frame) and the emu thread
//!    rendezvous-waits for that flush's handback, so the read-back sees the
//!    rasterized texels, never a pre-raster frame (real D3D9 semantics: the
//!    LockRect must expose what the preceding draws rendered). The implicit
//!    backbuffer is never handed back — the guest has no object for it, so
//!    it lives solely on the render thread. Depth buffers are never handed
//!    back either — no guest-visible read path exists for them.
//! 3. **Order is preserved by folding state into the ops.** There are only
//!    two op kinds: `Clear` (self-contained) and `Draw` (a full snapshot of
//!    the device state the draw consumes — matrices, viewport, render state,
//!    stage/sampler state + textures, shaders + constants, target bindings).
//!    The `Set*` handlers record nothing; each draw's snapshot IS the state
//!    its prefix of the stream produced, so a single ordered `Vec` replayed
//!    in order reproduces the emu-thread rasterization exactly. No
//!    reordering is possible by construction.
//!
//! # Legacy parity
//!
//! With the pipeline off (headless, CI, `WIE_CAPTURE_STREAM` unset) no op is
//! ever recorded and every handler takes the unchanged synchronous raster
//! path — the CI frame hashes exercise that path byte-for-byte. Validation
//! and return codes are identical on both paths (the capture branch sits
//! behind the same checks the raster path runs).

use std::sync::Arc;

use super::raster::{
    RasterFrame, build_fragment_state, fill_backbuffer_rect, primitive_groups, rasterize_frame,
};
use crate::WinApiState;
use crate::d3d9_render::{
    D3DRS_POINTSIZE, D3DTOP_DISABLE, FvfLayout, MAX_MIP_LEVELS, Mat4, MipChain, MipLevelView,
    PsProgram, RenderState, TextureStage, TextureStageState, Viewport, VsProgram, mat4_mul,
};
use crate::d3d9_shader::{PS_CONST_COUNT, PS_SAMPLER_COUNT, ParsedShader, ShaderKind};
use crate::d3d9_shader::{VS_BOOL_CONST_COUNT, VS_CONST_COUNT, VS_INT_CONST_COUNT};
use crate::gdi32::IRect;
use crate::handles::Hwnd;

// ── op types ─────────────────────────────────────────────────────────────

/// One captured clear (`Clear` op): everything the render thread needs to
/// reproduce the fill without touching `D3D9State`.
#[derive(Debug, Clone)]
pub(crate) struct CapturedClear {
    /// The bound render-target slot-0 surface VA (0 = the implicit backbuffer).
    pub rt: u64,
    /// The bound depth-stencil surface VA (0 = none).
    pub depth_stencil: u64,
    /// `D3DCLEAR_TARGET` was set.
    pub target: bool,
    /// `D3DCLEAR_ZBUFFER` was set.
    pub depth: bool,
    /// The masked 0RGB clear color.
    pub color: u32,
    /// The depth clear value (0.0 = near).
    pub z_value: f32,
    /// The explicit clear rects (empty = the whole target).
    pub rects: Vec<IRect>,
}

/// One captured texture: a private clone of a bound texture record's texels.
#[derive(Debug, Clone)]
pub(crate) struct CapturedTexture {
    /// Level-0 width in texels.
    pub width: u32,
    /// Level-0 height in texels.
    pub height: u32,
    /// The record's requested level count (drives `MipChain::count` — see
    /// `raster::resolve_sampler_stage`).
    pub levels: u32,
    /// Level-0 texels (`0xAARRGGBB`).
    pub pixels: Vec<u32>,
    /// Levels 1.. (the halved mip chain): `(width, height, texels)`.
    pub mips: Vec<(u32, u32, Vec<u32>)>,
}

/// One captured draw op: the stream bytes plus a full snapshot of the device
/// state the draw consumes (see the module doc — the `Set*` ops fold into
/// this snapshot).
#[derive(Clone, Debug)]
#[allow(clippy::type_complexity)]
pub(crate) struct CapturedDraw {
    /// The bound render-target slot-0 surface VA (0 = the implicit backbuffer).
    pub rt: u64,
    /// The bound depth-stencil surface VA (0 = none).
    pub depth_stencil: u64,
    /// Vertex pool bytes (the UP form's guest read, or the buffer form's
    /// stream slice — both read at record time).
    pub data: Vec<u8>,
    /// FVF decode layout.
    pub layout: FvfLayout,
    /// Stream stride in bytes.
    pub stride: usize,
    /// `D3DPT_*` primitive type.
    pub primitive_type: u64,
    /// Primitive count.
    pub primitive_count: u64,
    /// Index buffer bytes (`None` = non-indexed draw).
    pub indices: Option<Vec<u8>>,
    /// Index size (2 or 4 bytes).
    pub index_size: usize,
    /// `StartIndex` offset into the index buffer.
    pub index_offset: usize,
    /// `BaseVertexIndex` (a signed INT).
    pub vertex_base: i64,
    /// Fixed-function matrices (`D3DTS_WORLD/VIEW/PROJECTION` at record time).
    pub world: Mat4,
    pub view: Mat4,
    pub projection: Mat4,
    /// Viewport (`D3DVIEWPORT9` fields).
    pub viewport: (u32, u32, u32, u32, f32, f32),
    /// Typed render state + raw `D3DRS_POINTSIZE` + scissor rect.
    pub render_state: RenderState,
    pub point_size: f32,
    pub scissor: Option<IRect>,
    /// Per-stage texture state (TSS + sampler) snapshot.
    pub stage_states: [TextureStageState; 8],
    /// Per-stage bound texture snapshots (only bound stages carry one).
    pub textures: [Option<Box<CapturedTexture>>; 8],
    /// Bound pixel shader: the parsed program + float constants (`None` = FFP).
    pub ps: Option<(ParsedShader, [[f32; 4]; PS_CONST_COUNT])>,
    /// Bound vertex shader: the parsed program + constant files (`None` = FFP).
    pub vs: Option<(
        ParsedShader,
        [[f32; 4]; VS_CONST_COUNT],
        [[i32; 4]; VS_INT_CONST_COUNT],
        [bool; VS_BOOL_CONST_COUNT],
    )>,
}

/// One ordered stream op. Only two kinds exist — see the module doc on why
/// the `Set*` handlers record nothing.
#[derive(Debug, Clone)]
pub(crate) enum CaptureOp {
    /// A `Clear` fill.
    Clear(CapturedClear),
    /// A captured draw (boxed — by far the largest op variant).
    Draw(Box<CapturedDraw>),
}

// ── flush / handback messages ────────────────────────────────────────────

/// One Present flush: the ordered op stream plus the resource inputs the
/// render thread needs to (re)build its private target copies, and the
/// publish metadata the commit path carries (`present/commit.rs`).
pub(crate) struct CaptureFlush {
    /// Monotonic flush sequence (assigned by the pipeline's `enqueue`); the
    /// handback this flush's replay produces carries the same number — the
    /// render-target LockRect rendezvous waits on it.
    pub seq: u64,
    /// Device window that receives the frame.
    pub hwnd: Hwnd,
    /// Backbuffer dimensions (the guest render resolution).
    pub bb_w: u32,
    pub bb_h: u32,
    /// Device-window client size at flush time (the surface dims).
    pub win_w: u32,
    pub win_h: u32,
    /// The window's recorded class-brush background color (0RGB).
    pub background: u32,
    /// The ordered op stream (drained from `D3D9State::d3d9_capture_stream`).
    pub ops: Vec<CaptureOp>,
    /// Render-target texel inputs: `(surface VA, width, height, texels)`.
    /// Sent when the render thread has never seen the target or the emu
    /// thread mutated it since the last flush (see the module doc).
    pub rt_inputs: Vec<(u64, u32, u32, Arc<Vec<u32>>)>,
    /// Depth-stencil inputs, same protocol (initial copy only — the emu
    /// thread never mutates depth outside captured ops).
    pub depth_inputs: Vec<(u64, u32, u32, Arc<Vec<f32>>)>,
}

/// The render thread's post-frame target buffers, installed back into
/// `D3D9State` at the next flush boundary or render-target lock (see the
/// module doc — the sync-back half of the round-trip).
pub(crate) struct CaptureHandback {
    /// `(surface VA, texels)` for every render target the frame's ops
    /// referenced, in their post-replay state.
    pub rts: Vec<(u64, Arc<Vec<u32>>)>,
}

// ── gate + record helpers ────────────────────────────────────────────────

/// Whether the capture pipeline is live for this session (the render thread
/// spawned and the gate flipped). Every record/flush site checks this first —
/// when false, the legacy synchronous raster path runs unchanged.
pub(crate) fn capture_enabled(state: &mut WinApiState) -> bool {
    state.present().channel.capture_enabled()
}

/// Snapshot the per-stage bound textures (only stages with a live, non-empty
/// binding carry an entry — the same filter `resolve_sampler_stage` applies).
fn capture_textures(state: &mut WinApiState) -> [Option<Box<CapturedTexture>>; 8] {
    let d3d = state.d3d9();
    std::array::from_fn(|stage| {
        let binding = d3d.d3d9_texture_bindings.get(stage).copied().unwrap_or(0);
        if binding == 0 {
            return None;
        }
        d3d.d3d9_textures.get(&binding).and_then(|record| {
            if record.width == 0 || record.height == 0 || record.pixels.is_empty() {
                return None;
            }
            Some(Box::new(CapturedTexture {
                width: record.width,
                height: record.height,
                levels: record.levels,
                pixels: record.pixels.clone(),
                mips: record
                    .mip_levels
                    .iter()
                    .map(|mip| (mip.width, mip.height, mip.pixels.clone()))
                    .collect(),
            }))
        })
    })
}

/// Snapshot the bound shader pair (parsed programs + constant files).
#[allow(clippy::type_complexity)]
fn capture_programs(
    state: &mut WinApiState,
) -> (
    Option<(ParsedShader, [[f32; 4]; PS_CONST_COUNT])>,
    Option<(
        ParsedShader,
        [[f32; 4]; VS_CONST_COUNT],
        [[i32; 4]; VS_INT_CONST_COUNT],
        [bool; VS_BOOL_CONST_COUNT],
    )>,
) {
    let d3d = state.d3d9();
    let ps = if d3d.d3d9_pixel_shader == 0 {
        None
    } else {
        d3d.d3d9_shaders
            .get(&d3d.d3d9_pixel_shader)
            .and_then(|record| {
                (record.kind == ShaderKind::Pixel)
                    .then(|| (record.parsed.clone(), d3d.d3d9_ps_constants))
            })
    };
    let vs = if d3d.d3d9_current_vertex_shader == 0 {
        None
    } else {
        d3d.d3d9_shaders
            .get(&d3d.d3d9_current_vertex_shader)
            .and_then(|record| {
                (record.kind == ShaderKind::Vertex).then(|| {
                    (
                        record.parsed.clone(),
                        d3d.d3d9_vs_constants,
                        d3d.d3d9_vs_int_constants,
                        d3d.d3d9_vs_bool_constants,
                    )
                })
            })
    };
    (ps, vs)
}

/// Snapshot the per-draw device state every captured draw carries.
#[allow(clippy::type_complexity)]
fn capture_draw_state(
    state: &mut WinApiState,
) -> (
    u64,
    u64,
    Mat4,
    Mat4,
    Mat4,
    (u32, u32, u32, u32, f32, f32),
    RenderState,
    f32,
    Option<IRect>,
    [TextureStageState; 8],
) {
    let d3d = state.d3d9();
    let point_size = f32::from_bits(
        d3d.d3d9_render_state_raw
            .get(&D3DRS_POINTSIZE)
            .copied()
            .unwrap_or(1.0_f32.to_bits()),
    );
    (
        d3d.d3d9_render_target,
        d3d.d3d9_depth_stencil,
        d3d.d3d9_world_matrix,
        d3d.d3d9_view_matrix,
        d3d.d3d9_projection_matrix,
        d3d.d3d9_viewport,
        d3d.d3d9_render_state,
        point_size,
        d3d.d3d9_scissor_rect,
        d3d.d3d9_stage_states.clone(),
    )
}

/// Assemble the shared tail of the two record fns (everything but the stream
/// bytes) into a [`CapturedDraw`].
#[allow(clippy::too_many_arguments)]
fn build_captured_draw(
    state: &mut WinApiState,
    data: Vec<u8>,
    layout: FvfLayout,
    stride: usize,
    primitive_type: u64,
    primitive_count: u64,
    indices: Option<Vec<u8>>,
    index_size: usize,
    index_offset: usize,
    vertex_base: i64,
) -> CapturedDraw {
    let (
        rt,
        depth_stencil,
        world,
        view,
        projection,
        viewport,
        render_state,
        point_size,
        scissor,
        stage_states,
    ) = capture_draw_state(state);
    let (ps, vs) = capture_programs(state);
    CapturedDraw {
        rt,
        depth_stencil,
        data,
        layout,
        stride,
        primitive_type,
        primitive_count,
        indices,
        index_size,
        index_offset,
        vertex_base,
        world,
        view,
        projection,
        viewport,
        render_state,
        point_size,
        scissor,
        stage_states,
        textures: capture_textures(state),
        ps,
        vs,
    }
}

// ── record fns (called by the draw/clear handlers) ───────────────────────

/// Record a `Draw*UP` op: read the guest vertex (and optional index) bytes at
/// record time and append the draw.
///
/// Mirrors `raster::draw_vertex_stream`'s guards exactly: a zero-sized draw
/// or an unreadable guest buffer records nothing (the legacy path draws no
/// pixels — parity by construction).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_draw_up(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    layout: FvfLayout,
    stride: usize,
    vertex_count: usize,
    primitive_type: u64,
    primitive_count: u64,
    data_va: u64,
    index_va: u64,
    index_format: u32,
    index_count: usize,
) {
    if vertex_count == 0 || stride == 0 || data_va == 0 {
        return;
    }
    let Ok(groups) = primitive_groups(primitive_type, primitive_count) else {
        return;
    };
    if groups.is_empty() {
        return;
    }
    let Some(data_bytes) = vertex_count.checked_mul(stride) else {
        return; // size overflow — the legacy path aborts with no pixels too
    };
    let index_info = if index_count > 0 && index_va != 0 {
        let size = usize::from(index_format == crate::d3d9::D3DFMT_INDEX32).saturating_mul(4)
            + usize::from(index_format != crate::d3d9::D3DFMT_INDEX32).saturating_mul(2);
        index_count.checked_mul(size).map(|bytes| (size, bytes))
    } else {
        None
    };
    let mut data = vec![0_u8; data_bytes];
    if engine.mem_read(data_va, &mut data).is_err() {
        return; // unmapped guest memory: skip the draw, no pixels
    }
    let indices = index_info.and_then(|(size, index_bytes)| {
        let mut bytes = vec![0_u8; index_bytes];
        match engine.mem_read(index_va, &mut bytes) {
            Ok(()) => Some((bytes, size)),
            Err(_) => None,
        }
    });
    let (indices, index_size) = match indices {
        Some((bytes, size)) => (Some(bytes), size),
        None => (None, 0),
    };
    let draw = build_captured_draw(
        state,
        data,
        layout,
        stride,
        primitive_type,
        primitive_count,
        indices,
        index_size,
        0,
        0,
    );
    state
        .d3d9()
        .d3d9_capture_stream
        .push(CaptureOp::Draw(Box::new(draw)));
}

/// Record a buffer-form draw (`DrawPrimitive`/`DrawIndexedPrimitive`): the
/// caller already cloned the stream + index bytes out of the buffer records —
/// this moves them into the op with the state snapshot.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_draw_buffer(
    state: &mut WinApiState,
    data: Vec<u8>,
    layout: FvfLayout,
    stride: usize,
    primitive_type: u64,
    primitive_count: u64,
    indices: Option<Vec<u8>>,
    index_size: usize,
    index_offset: usize,
    vertex_base: i64,
) {
    let draw = build_captured_draw(
        state,
        data,
        layout,
        stride,
        primitive_type,
        primitive_count,
        indices,
        index_size,
        index_offset,
        vertex_base,
    );
    state
        .d3d9()
        .d3d9_capture_stream
        .push(CaptureOp::Draw(Box::new(draw)));
}

/// Record a `Clear` op (the caller parsed the guest rects already).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_clear(
    state: &mut WinApiState,
    rt: u64,
    depth_stencil: u64,
    target: bool,
    depth: bool,
    color: u32,
    z_value: f32,
    rects: Vec<IRect>,
) {
    state
        .d3d9()
        .d3d9_capture_stream
        .push(CaptureOp::Clear(CapturedClear {
            rt,
            depth_stencil,
            target,
            depth,
            color,
            z_value,
            rects,
        }));
}

// ── flush (Present) + handback install ───────────────────────────────────

/// Drain a pending render-thread handback and install its target buffers into
/// `D3D9State` — the sync point every render-target lock/flush takes first
/// (see the module doc's round-trip protocol).
pub(crate) fn drain_handback(state: &mut WinApiState) {
    let Some(handback) = state.present().channel.take_capture_handback() else {
        return;
    };
    let d3d = state.d3d9();
    for (va, pixels) in handback.rts {
        if let Some(record) = d3d.d3d9_render_targets.get_mut(&va) {
            // Replace the storage wholesale: the handback is the render
            // thread's post-frame authority for this target.
            record.pixels = pixels;
        }
    }
}

/// Flush the op stream at `Present`: drain the handback, collect the target
/// inputs the render thread needs, move the stream into a [`CaptureFlush`],
/// and enqueue it on the channel.
pub(crate) fn flush_present(state: &mut WinApiState, win_w: u32, win_h: u32) {
    let flush = build_flush(state, win_w, win_h);
    state.present().channel.enqueue_capture_flush(flush);
}

/// Synchronous rendezvous for guest render-target READ-BACK: the replay only
/// runs on the render thread at flush boundaries, so a guest that renders
/// into an offscreen RT and `LockRect`s it BEFORE any `Present` (the gui_d3d9
/// L6 self-test) would read empty texels. This drains any handback, and when
/// ops are pending flushes them right now with zero publish dims (the render
/// thread replays + posts the handback and skips the frame publish), then
/// waits for that flush's handback so the emu-side target texels are the
/// rasterized ones the guest expects. Rare path (RT locks with a non-empty
/// stream) — the rendezvous cost is fine there.
pub(crate) fn sync_flush_for_target_read(state: &mut WinApiState) {
    drain_handback(state);
    if state.d3d9().d3d9_capture_stream.is_empty() {
        return;
    }
    // win_w/win_h = 0: the render thread replays + posts the handback but
    // publishes no frame (nothing was Presented).
    let flush = build_flush(state, 0, 0);
    let seq = state.present().channel.enqueue_capture_flush(flush);
    state
        .present()
        .channel
        .capture_wait_for_handback(seq, std::time::Duration::from_secs(2));
    drain_handback(state);
}

/// Collect the pending op stream + the target inputs it references into one
/// flush (the Publish-dims and rendezvous callers share this).
fn build_flush(state: &mut WinApiState, win_w: u32, win_h: u32) -> CaptureFlush {
    let (bb_w, bb_h, hwnd) = {
        let d3d = state.d3d9();
        (
            d3d.d3d9_backbuffer_width,
            d3d.d3d9_backbuffer_height,
            d3d.d3d9_present_hwnd,
        )
    };
    let ops = std::mem::take(&mut state.d3d9().d3d9_capture_stream);
    // Collect the targets the stream references, in first-reference order.
    let mut referenced_rts: Vec<u64> = Vec::new();
    let mut referenced_depths: Vec<u64> = Vec::new();
    for op in &ops {
        let (rt, depth_stencil) = match op {
            CaptureOp::Clear(clear) => (clear.rt, clear.depth_stencil),
            CaptureOp::Draw(draw) => (draw.rt, draw.depth_stencil),
        };
        if rt != 0 && !referenced_rts.contains(&rt) {
            referenced_rts.push(rt);
        }
        if depth_stencil != 0 && !referenced_depths.contains(&depth_stencil) {
            referenced_depths.push(depth_stencil);
        }
    }
    let mut rt_inputs = Vec::new();
    let mut depth_inputs = Vec::new();
    {
        let d3d = state.d3d9();
        for va in referenced_rts {
            let never_sent = d3d.d3d9_capture_rt_seen.insert(va);
            let emu_dirty = d3d.d3d9_capture_emu_dirty_rt.remove(&va);
            if (never_sent || emu_dirty)
                && let Some(record) = d3d.d3d9_render_targets.get(&va)
            {
                rt_inputs.push((va, record.width, record.height, Arc::clone(&record.pixels)));
            }
        }
        for va in referenced_depths {
            if d3d.d3d9_capture_depth_seen.insert(va)
                && let Some(record) = d3d.d3d9_depth_surfaces.get(&va)
            {
                depth_inputs.push((va, record.width, record.height, Arc::clone(&record.depth)));
            }
        }
    }
    let background = state
        .present()
        .background_colors
        .get(&hwnd)
        .copied()
        .unwrap_or(crate::present::DEFAULT_BACKGROUND_COLOR);
    CaptureFlush {
        // The pipeline's enqueue assigns the real sequence number.
        seq: 0,
        hwnd,
        bb_w,
        bb_h,
        win_w,
        win_h,
        background,
        ops,
        rt_inputs,
        depth_inputs,
    }
}

// ── render-thread replay ─────────────────────────────────────────────────

/// Resolve one captured stage's sampler state against the captured textures —
/// the replay-side mirror of `raster::resolve_sampler_stage`.
fn captured_sampler_stage<'a>(
    stage_idx: usize,
    stages: &'a [TextureStageState; 8],
    textures: &'a [Option<Box<CapturedTexture>>; 8],
) -> Option<TextureStage<'a>> {
    let texture = textures.get(stage_idx)?.as_ref()?;
    let stage = stages.get(stage_idx)?;
    if texture.width == 0 || texture.height == 0 || texture.pixels.is_empty() {
        return None;
    }
    let mut chain = MipChain {
        count: 0,
        levels: [None; MAX_MIP_LEVELS],
    };
    if let Some(slot) = chain.levels.first_mut() {
        *slot = Some(MipLevelView {
            width: texture.width,
            height: texture.height,
            pixels: &texture.pixels,
        });
    }
    chain.count = texture.levels;
    for (index, (width, height, pixels)) in texture.mips.iter().enumerate() {
        if let Some(slot) = chain.levels.get_mut(index.saturating_add(1)) {
            *slot = Some(MipLevelView {
                width: *width,
                height: *height,
                pixels,
            });
        }
    }
    Some(TextureStage {
        pixels: &texture.pixels,
        width: texture.width,
        height: texture.height,
        addr_u: stage.address_u,
        addr_v: stage.address_v,
        mag_filter: stage.mag_filter,
        min_filter: stage.min_filter,
        mip_filter: stage.mip_filter,
        mips: chain,
        color_op: stage.color_op,
        color_arg1: stage.color_arg1,
        color_arg2: stage.color_arg2,
        alpha_op: stage.alpha_op,
        alpha_arg1: stage.alpha_arg1,
        alpha_arg2: stage.alpha_arg2,
    })
}

/// The stage-0 FFP sampler (`None` when untextured or the stage is disabled) —
/// the replay-side mirror of `raster::resolve_texture_stage`.
fn captured_ffp_texture<'a>(
    stages: &'a [TextureStageState; 8],
    textures: &'a [Option<Box<CapturedTexture>>; 8],
) -> Option<TextureStage<'a>> {
    let stage = captured_sampler_stage(0, stages, textures)?;
    if stage.color_op == D3DTOP_DISABLE {
        return None;
    }
    Some(stage)
}

/// The `s0..s3` sampler registers for a bound pixel shader.
fn captured_ps_samplers<'a>(
    stages: &'a [TextureStageState; 8],
    textures: &'a [Option<Box<CapturedTexture>>; 8],
) -> [Option<TextureStage<'a>>; PS_SAMPLER_COUNT] {
    std::array::from_fn(|index| captured_sampler_stage(index, stages, textures))
}

/// The render thread's private target set: the implicit backbuffer plus its
/// own copies of every render target / depth buffer the streams reference.
///
/// `apply_flush` absorbs the flush's inputs (replacing a copy whenever the
/// emu thread is authoritative), `replay` runs the ops, `take_handback`
/// snapshots the touched targets back. All targets are owned `Vec`s — the
/// replay mutates them in place.
#[derive(Default)]
pub(crate) struct ReplayTargets {
    /// The implicit backbuffer (0RGB, `bb_w * bb_h` words).
    backbuffer: Vec<u32>,
    /// The implicit backbuffer dimensions (from the current flush).
    bb_w: u32,
    bb_h: u32,
    /// Render-target copies keyed by surface VA: `(width, height, texels)`.
    rts: ahash::HashMap<u64, (u32, u32, Vec<u32>)>,
    /// Depth-stencil copies keyed by surface VA: `(width, height, depth)`.
    depths: ahash::HashMap<u64, (u32, u32, Vec<f32>)>,
    /// Targets referenced by the current flush (the handback's contents).
    touched_rts: Vec<u64>,
}

impl ReplayTargets {
    /// Absorb one flush's resource inputs: resize the implicit backbuffer,
    /// replace/augment the target copies. Inputs carry `Arc` snapshots —
    /// owned copies are taken (unwrapping the `Arc` when the emu thread no
    /// longer shares it, cloning otherwise).
    pub(crate) fn apply_flush(&mut self, flush: &CaptureFlush) {
        let needed = usize::try_from(flush.bb_w.checked_mul(flush.bb_h).unwrap_or(0)).unwrap_or(0);
        if self.backbuffer.len() != needed {
            // A dimension change resets the frame (the legacy CreateDevice
            // zero-fills the backbuffer too).
            self.backbuffer = vec![0_u32; needed];
        }
        self.bb_w = flush.bb_w;
        self.bb_h = flush.bb_h;
        self.touched_rts.clear();
        for (va, width, height, pixels) in &flush.rt_inputs {
            let needed = usize::try_from(width.checked_mul(*height).unwrap_or(0)).unwrap_or(0);
            let texels = match Arc::try_unwrap(Arc::clone(pixels)) {
                Ok(vec) => vec,
                Err(shared) => (*shared).clone(),
            };
            if texels.len() != needed {
                continue; // stale input for a target whose dims no longer match
            }
            self.rts.insert(*va, (*width, *height, texels));
        }
        for (va, width, height, depth) in &flush.depth_inputs {
            let needed = usize::try_from(width.checked_mul(*height).unwrap_or(0)).unwrap_or(0);
            let texels = match Arc::try_unwrap(Arc::clone(depth)) {
                Ok(vec) => vec,
                Err(shared) => (*shared).clone(),
            };
            if texels.len() != needed {
                continue;
            }
            self.depths.insert(*va, (*width, *height, texels));
        }
    }

    /// Replay the flush's op stream into the private targets, in order.
    pub(crate) fn replay(&mut self, ops: &[CaptureOp]) {
        for op in ops {
            match op {
                CaptureOp::Clear(clear) => self.replay_clear(clear),
                CaptureOp::Draw(draw) => self.replay_draw(draw),
            }
        }
    }

    /// Resolve the color output for an op: `(width, height, texels)` for the
    /// bound render target's copy, or the implicit backbuffer. `None` for a
    /// stale target binding (no copy exists — the same skip the emu path
    /// takes). The texel storage is returned as the owned `Vec` so a stale
    /// size can be re-filled in place (the legacy Clear's resize semantics).
    fn resolve_color_output(&mut self, rt: u64) -> Option<(u32, u32, &mut Vec<u32>)> {
        if rt == 0 {
            let (w, h) = (self.bb_w, self.bb_h);
            return Some((w, h, &mut self.backbuffer));
        }
        self.rts
            .get_mut(&rt)
            .map(|(width, height, texels)| (*width, *height, texels))
    }

    /// `Clear` replay: the depth fill first (the emu handler's order), then
    /// the color fills (resize-with-color when the buffer is the wrong size,
    /// then the explicit rects or the full fill).
    fn replay_clear(&mut self, clear: &CapturedClear) {
        if clear.depth
            && let Some((_, _, texels)) = self.depths.get_mut(&clear.depth_stencil)
        {
            for slot in texels {
                *slot = clear.z_value;
            }
        }
        if !clear.target {
            return;
        }
        let Some((width, height, texels)) = self.resolve_color_output(clear.rt) else {
            return;
        };
        let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
        if texels.len() != needed {
            // The legacy Clear re-allocates the target filled with the clear
            // color when its buffer size is stale; mirror that.
            *texels = vec![clear.color; needed];
            return;
        }
        if clear.rects.is_empty() {
            texels.fill(clear.color);
        } else {
            for rect in &clear.rects {
                fill_backbuffer_rect(
                    texels.as_mut_slice(),
                    width,
                    height,
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    clear.color,
                );
            }
        }
        if clear.rt != 0 {
            self.touched_rts.push(clear.rt);
        }
    }

    /// `Draw` replay: resolve the target + depth + captured state into a
    /// [`RasterFrame`] and run the shared rasterizer core.
    fn replay_draw(&mut self, draw: &CapturedDraw) {
        let Ok(groups) = primitive_groups(draw.primitive_type, draw.primitive_count) else {
            return;
        };
        if groups.is_empty() || draw.data.is_empty() || draw.stride == 0 {
            return;
        }
        // Resolve the color output: the bound target's copy, or the implicit
        // backbuffer. A stale target binding (no copy) draws nothing — the
        // same skip the emu path takes. Direct field borrows keep this
        // disjoint from the depth borrow below.
        let (width, height, output) = if draw.rt == 0 {
            (self.bb_w, self.bb_h, &mut self.backbuffer)
        } else {
            match self.rts.get_mut(&draw.rt) {
                Some((width, height, texels)) => (*width, *height, texels),
                None => return,
            }
        };
        // The bound depth buffer's copy (`None` when unbound or stale — the
        // draw runs depth-less, exactly like the emu path's missing record).
        let depth = match self.depths.get_mut(&draw.depth_stencil) {
            Some((_, _, texels)) if draw.depth_stencil != 0 => Some(texels.as_mut_slice()),
            _ => None,
        };
        let frag = build_fragment_state(&draw.render_state, depth, draw.scissor);
        let matrix = mat4_mul(&draw.world, &mat4_mul(&draw.view, &draw.projection));
        let (vp_x, vp_y, vp_w, vp_h, vp_min_z, vp_max_z) = draw.viewport;
        let viewport = Viewport {
            x: vp_x,
            y: vp_y,
            width: vp_w,
            height: vp_h,
            min_z: vp_min_z,
            max_z: vp_max_z,
        };
        let tex = captured_ffp_texture(&draw.stage_states, &draw.textures);
        let samplers = captured_ps_samplers(&draw.stage_states, &draw.textures);
        let ps = draw.ps.as_ref().map(|(parsed, constants)| {
            let mut sampler_refs: [Option<&TextureStage<'_>>; PS_SAMPLER_COUNT] =
                [None; PS_SAMPLER_COUNT];
            for (index, slot) in sampler_refs.iter_mut().enumerate() {
                *slot = samplers.get(index).and_then(|s| s.as_ref());
            }
            PsProgram {
                instructions: &parsed.instructions,
                constants: *constants,
                samplers: sampler_refs,
            }
        });
        let vs =
            draw.vs.as_ref().map(
                |(parsed, constants, int_constants, bool_constants)| VsProgram {
                    instructions: &parsed.instructions,
                    constants: *constants,
                    int_constants: *int_constants,
                    bool_constants: *bool_constants,
                },
            );
        let indices = draw
            .indices
            .as_deref()
            .map(|bytes| (bytes, draw.index_size, draw.index_offset));
        let mut frame = RasterFrame {
            output: output.as_mut_slice(),
            width,
            height,
            matrix,
            viewport,
            pre_transformed: draw.layout.pre_transformed,
            point_size: draw.point_size,
            tex,
            ps,
            vs,
            frag,
            data: &draw.data,
            layout: &draw.layout,
            stride: draw.stride,
            groups: &groups,
            indices,
            vertex_base: draw.vertex_base,
            dirty: None,
        };
        rasterize_frame(&mut frame);
        if draw.rt != 0 {
            self.touched_rts.push(draw.rt);
        }
    }

    /// Read-only view of the implicit backbuffer (the render thread's
    /// publish source).
    pub(crate) fn backbuffer(&self) -> &[u32] {
        &self.backbuffer
    }

    /// Snapshot the touched targets' post-frame state for the handback.
    pub(crate) fn take_handback(&mut self) -> CaptureHandback {
        let rts = self
            .touched_rts
            .iter()
            .filter_map(|va| {
                self.rts
                    .get(va)
                    .map(|(_, _, texels)| (*va, Arc::new(texels.clone())))
            })
            .collect();
        self.touched_rts.clear();
        CaptureHandback { rts }
    }
}
