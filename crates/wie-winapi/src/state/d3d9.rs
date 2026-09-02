//! Direct3D 9 rendering state.
//!
//! All fields are `pub(crate)`: D3D9 state is reachable from handlers through
//! `WinApiState::d3d9()`, and no external crate reads these fields directly.

use ahash::HashMap;
use ahash::HashMapExt;

/// Whether a D3D9 scene is in progress (`BeginScene` … `EndScene`).
///
/// A two-variant enum instead of a bare `bool` so every read site states which
/// state it means. Mirrors the device's own behavior: `EndScene` while
/// `Inactive` proceeds silently (kept exactly as the previous `bool` check).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SceneState {
    /// No scene in progress (or a stray `EndScene`).
    #[default]
    Inactive,
    /// `BeginScene` succeeded and `EndScene` has not run.
    Active,
}

/// Direct3D 9 rendering state.
#[derive(Debug, Clone)]
pub struct D3D9State {
    pub(crate) d3d9_current_vertex_shader: u64,
    pub(crate) d3d9_current_fvf: u32,
    /// Typed device render state (`D3DRS_*`), decoded at the Set/GetRenderState
    /// register boundary.
    pub(crate) d3d9_render_state: crate::d3d9_render::RenderState,
    /// Last-set raw values for unmodeled `D3DRS_*` states.
    ///
    /// The round-trip fidelity layer: `GetRenderState` returns exactly what
    /// the guest last set — modeled states read the typed struct, ignored
    /// states read this map (0 when never set).
    pub(crate) d3d9_render_state_raw: HashMap<u32, u32>,
    /// The `SetScissorRect` rect in backbuffer coords (None = never set —
    /// the scissor test never clips).
    pub(crate) d3d9_scissor_rect: Option<crate::gdi32::IRect>,
    /// Fixed-function texture-space transforms (`D3DTS_TEXTURE0..7`), stored
    /// for the `Set/GetTransform` + `MultiplyTransform` round-trips; the
    /// actual texgen runs in a later slice.
    pub(crate) d3d9_texture_matrices: [[f32; 16]; 8],
    /// Per-stage texture state (TSS + sampler), indexed by stage (0..8).
    pub(crate) d3d9_stage_states: [crate::d3d9_render::TextureStageState; 8],
    pub(crate) d3d9_device_object_address: u64,
    pub(crate) d3d9_device_ref_count: u32,
    pub(crate) d3d9_object_address: u64,
    pub(crate) d3d9_ref_count: u32,
    // ── P3 software-render state (slice 1) ──────────────────────────────
    /// Backbuffer size (from the CreateDevice presentation parameters).
    pub(crate) d3d9_backbuffer_width: u32,
    /// Backbuffer size (from the CreateDevice presentation parameters).
    pub(crate) d3d9_backbuffer_height: u32,
    /// Host-owned 0RGB backbuffer (top-down), the frame the guest renders into.
    ///
    /// Chosen over a guest-visible block: every write in slice 1 comes from a
    /// host-side handler (Clear/Draw*), so the guest never touches this
    /// memory directly and the rasterizer owns the buffer outright.
    pub(crate) d3d9_backbuffer: Vec<u32>,
    /// Window handle that receives Present frames (hDeviceWindow).
    pub(crate) d3d9_present_hwnd: crate::handles::Hwnd,
    /// Whether BeginScene has been called (and EndScene has not).
    pub(crate) d3d9_scene_active: SceneState,
    /// Fixed-function world matrix (`D3DTS_WORLD`).
    pub(crate) d3d9_world_matrix: [f32; 16],
    /// Fixed-function view matrix (`D3DTS_VIEW`).
    pub(crate) d3d9_view_matrix: [f32; 16],
    /// Fixed-function projection matrix (`D3DTS_PROJECTION`).
    pub(crate) d3d9_projection_matrix: [f32; 16],
    /// Viewport (`D3DVIEWPORT9` fields: X, Y, Width, Height, MinZ, MaxZ).
    pub(crate) d3d9_viewport: (u32, u32, u32, u32, f32, f32),
    /// Accumulated dirty region since the last Present (backbuffer coords);
    /// `None` = the whole backbuffer changed (Clear always sets this).
    pub(crate) d3d9_dirty: Option<crate::gdi32::IRect>,
    /// `SetStreamSource` stream-0 vertex buffer pointer (buffer form).
    pub(crate) d3d9_stream_source_va: u64,
    /// `SetStreamSource` stream-0 stride.
    pub(crate) d3d9_stream_stride: u32,
    /// `SetStreamSource` stream-0 `OffsetInBytes` (added to the buffer base
    /// when the buffer-form draws resolve vertex positions).
    pub(crate) d3d9_stream_offset: u32,
    /// `SetIndices` index buffer pointer (buffer form; unused in slice 1).
    pub(crate) d3d9_index_buffer_va: u64,
    // ── L2 buffer-object state ──────────────────────────────────────────
    /// Vertex/index buffer records keyed by the buffer object's guest VA.
    pub(crate) d3d9_buffers: HashMap<u64, crate::d3d9::BufferRecord>,
    // ── P4b texture state ───────────────────────────────────────────────
    /// Texture records keyed by the texture object's guest VA.
    pub(crate) d3d9_textures: HashMap<u64, crate::d3d9::TextureRecord>,
    /// Surface object VA → texture object VA (`GetSurfaceLevel` views).
    pub(crate) d3d9_surface_textures: HashMap<u64, u64>,
    /// Surface object VA → the mip level it views (L4: `GetSurfaceLevel` is
    /// level-parameterized — the surface form of `LockRect` resolves which
    /// level's texels the lock targets).
    pub(crate) d3d9_surface_levels: HashMap<u64, u32>,
    /// Per-stage (0..8) texture binding (0 = none).
    pub(crate) d3d9_texture_bindings: [u64; 8],
    // ── L6 render-target state ─────────────────────────────────────────
    /// Offscreen render targets keyed by the surface object's guest VA.
    /// Each holds its own host texel buffer (A8R8G8B8), so `SetRenderTarget`
    /// + `Clear`/draws can render offscreen (post-processing, shadow maps).
    pub(crate) d3d9_render_targets: HashMap<u64, crate::d3d9::RenderTargetRecord>,
    /// Bound render target slot 0's surface VA (0 = the implicit backbuffer).
    pub(crate) d3d9_render_target: u64,
    // ── depth-stencil state ────────────────────────────────────────────
    /// Depth-stencil surfaces keyed by the surface object's guest VA.
    pub(crate) d3d9_depth_surfaces: HashMap<u64, crate::d3d9::DepthStencilRecord>,
    /// Bound depth-stencil surface VA (0 = none).
    pub(crate) d3d9_depth_stencil: u64,
    // ── shader state ───────────────────────────────────────────────────
    /// Pixel/vertex shader records keyed by the shader object's guest VA.
    pub(crate) d3d9_shaders: HashMap<u64, crate::d3d9_shader::ShaderRecord>,
    /// Bound pixel shader VA (0 = none — the FFP path runs).
    pub(crate) d3d9_pixel_shader: u64,
    /// Pixel-shader constant registers `c0..c31` (float4, set via
    /// `SetPixelShaderConstantF`; `def` at Create* time writes these too).
    pub(crate) d3d9_ps_constants: [[f32; 4]; crate::d3d9_shader::PS_CONST_COUNT],
    /// Vertex-shader constant registers `c0..c255` (stored; the vertex stage
    /// stays FFP until vertex shaders execute).
    pub(crate) d3d9_vs_constants: [[f32; 4]; crate::d3d9_shader::VS_CONST_COUNT],
    /// Vertex-shader integer constant registers `i0..i3` (int4; set via
    /// `SetVertexShaderConstantI` + `defi` at Create time; the `loop`/`rep`
    /// sources read these).
    pub(crate) d3d9_vs_int_constants: [[i32; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT],
    /// Vertex-shader boolean constant registers `b0..b15` (set via
    /// `SetVertexShaderConstantB` + `defb` at Create time).
    pub(crate) d3d9_vs_bool_constants: [bool; crate::d3d9_shader::VS_BOOL_CONST_COUNT],
    // ── Q9/C pooled target + scratch ──────────────────────────────────────
    /// Q9/C: per-device texture mip-chain pool — `tex handle → Vec<Vec<u32>>` chain
    /// (level0 + mips) with capacity retained; CreateTexture reuses, Release returns.
    pub(crate) d3d9_texture_pool: HashMap<u64, Vec<Vec<u32>>>,
    /// Q9/C: per-Device9 tiled scratch for raster scanline/blended spans — one
    /// reused `Vec<u32>` per device, not a per-draw `vec![]`.
    pub(crate) d3d9_tile_scratch: Vec<u32>,
    /// Q9/C: per-Device9 vertex/index scratch — reused `Vec<u8>` for
    /// `Draw*UP` guest reads, not a per-draw `vec![0_u8; n]`.
    pub(crate) d3d9_vertex_scratch: Vec<u8>,
    /// Opt-in present pacing (`WIE_PRESENT_PACING_HZ`): the last Present
    /// instant, so the next Present can sleep out the remainder of the
    /// target frame interval. Diagnostic only — the sleep runs under the
    /// WinAPI lock.
    pub(crate) d3d9_last_present: Option<std::time::Instant>,
}

impl Default for D3D9State {
    fn default() -> Self {
        Self {
            d3d9_current_vertex_shader: 0,
            d3d9_current_fvf: 0,
            d3d9_render_state: crate::d3d9_render::RenderState::default(),
            d3d9_render_state_raw: HashMap::new(),
            d3d9_scissor_rect: None,
            d3d9_texture_matrices: [crate::d3d9_render::IDENTITY; 8],
            d3d9_stage_states: std::array::from_fn(|_| {
                crate::d3d9_render::TextureStageState::default()
            }),
            d3d9_device_object_address: 0,
            d3d9_device_ref_count: 0,
            d3d9_object_address: 0,
            d3d9_ref_count: 0,
            d3d9_backbuffer_width: 0,
            d3d9_backbuffer_height: 0,
            d3d9_backbuffer: Vec::new(),
            d3d9_present_hwnd: crate::handles::Hwnd::NULL,
            d3d9_scene_active: SceneState::Inactive,
            // D3D9's default transform state is the identity matrix (a zero
            // matrix would map every vertex to w=0 and reject all draws).
            d3d9_world_matrix: crate::d3d9_render::IDENTITY,
            d3d9_view_matrix: crate::d3d9_render::IDENTITY,
            d3d9_projection_matrix: crate::d3d9_render::IDENTITY,
            d3d9_viewport: (0, 0, 0, 0, 0.0, 1.0),
            d3d9_dirty: None,
            d3d9_stream_source_va: 0,
            d3d9_stream_stride: 0,
            d3d9_stream_offset: 0,
            d3d9_index_buffer_va: 0,
            d3d9_buffers: HashMap::new(),
            d3d9_textures: HashMap::new(),
            d3d9_surface_textures: HashMap::new(),
            d3d9_surface_levels: HashMap::new(),
            d3d9_texture_bindings: [0; 8],
            d3d9_render_targets: HashMap::new(),
            d3d9_render_target: 0,
            d3d9_depth_surfaces: HashMap::new(),
            d3d9_depth_stencil: 0,
            d3d9_shaders: HashMap::new(),
            d3d9_pixel_shader: 0,
            d3d9_ps_constants: [[0.0; 4]; crate::d3d9_shader::PS_CONST_COUNT],
            d3d9_vs_constants: [[0.0; 4]; crate::d3d9_shader::VS_CONST_COUNT],
            d3d9_vs_int_constants: [[0; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT],
            d3d9_vs_bool_constants: [false; crate::d3d9_shader::VS_BOOL_CONST_COUNT],
            d3d9_texture_pool: HashMap::with_capacity(16),
            d3d9_tile_scratch: Vec::new(),
            d3d9_vertex_scratch: Vec::new(),
            d3d9_last_present: None,
        }
    }
}
