//! P3: D3D9 software-render core — FVF layout parsing, vertex transform,
//! and solid-fill triangle rasterization.
//!
//! Slice 1 scope (roadmap B6 slice 1): `D3DFVF_XYZ` / `D3DFVF_XYZRHW`
//! positions, optional `D3DFVF_NORMAL` / `D3DFVF_DIFFUSE` / `D3DFVF_SPECULAR`,
//! flat-vertex diffuse colors, a fixed-function world × view × projection
//! transform, near-plane rejection, and a top-left-rule fill with barycentric
//! color interpolation. No textures, lighting, or z-buffer (draw order).
//! P5a adds the PS 2.0 interpreter in the fragment stage (below).

use crate::d3d9_shader::{
    D3DSPDM_SATURATE, D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG, D3DSPSM_COMP,
    D3DSPSM_NEG, D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2, D3DSPSM_X2NEG, Operand, PS_CONST_COUNT,
    PS_INPUT_COUNT, PS_SAMPLER_COUNT, PS_TEMP_COUNT, PsInstruction, PsOp, RegType,
};
use crate::gdi32::IRect;
use wie_cpu::{blend_0rgb_4x, fill_0rgb_4x};

/// `D3DFVF_XYZ`: untransformed position (3 floats).
pub const D3DFVF_XYZ: u32 = 0x0002;
/// `D3DFVF_XYZRHW`: pre-transformed position (4 floats, screen space).
pub const D3DFVF_XYZRHW: u32 = 0x0004;
/// `D3DFVF_NORMAL`: vertex normal (3 floats; unused in slice 1).
pub const D3DFVF_NORMAL: u32 = 0x0010;
/// `D3DFVF_DIFFUSE`: vertex color (4 bytes `0xAARRGGBB`).
pub const D3DFVF_DIFFUSE: u32 = 0x0040;
/// `D3DFVF_SPECULAR`: specular color (4 bytes; unused in slice 1).
pub const D3DFVF_SPECULAR: u32 = 0x0080;
/// `D3DFVF_TEX1`: first texture-coordinate set (2 floats; unused).
pub const D3DFVF_TEX1: u32 = 0x0100;
/// `D3DFVF_TEX8`: last texture-coordinate set (the `TEX1..TEX8` bit mask).
pub const D3DFVF_TEX8: u32 = 0x8000;

/// `D3DPT_TRIANGLELIST`.
pub const D3DPT_TRIANGLELIST: u32 = 4;
/// `D3DPT_TRIANGLESTRIP`.
pub const D3DPT_TRIANGLESTRIP: u32 = 5;
/// `D3DPT_TRIANGLEFAN`.
pub const D3DPT_TRIANGLEFAN: u32 = 6;

/// `D3DTS_VIEW`.
pub const D3DTS_VIEW: u32 = 2;
/// `D3DTS_PROJECTION`.
pub const D3DTS_PROJECTION: u32 = 3;
/// `D3DTS_WORLD` (world matrix index 0).
pub const D3DTS_WORLD: u32 = 256;

// ── Texture-stage constants (d3d9types.h values) ────────────────────────

/// `D3DTSS_COLOROP` (stage state type).
pub const D3DTSS_COLOROP: u32 = 1;
/// `D3DTSS_COLORARG1`.
pub const D3DTSS_COLORARG1: u32 = 2;
/// `D3DTSS_COLORARG2`.
pub const D3DTSS_COLORARG2: u32 = 3;
/// `D3DTSS_ALPHAOP`.
pub const D3DTSS_ALPHAOP: u32 = 4;
/// `D3DTSS_ALPHAARG1`.
pub const D3DTSS_ALPHAARG1: u32 = 5;
/// `D3DTSS_ALPHAARG2`.
pub const D3DTSS_ALPHAARG2: u32 = 6;
/// `D3DTSS_TEXCOORDINDEX`.
pub const D3DTSS_TEXCOORDINDEX: u32 = 11;

/// `D3DSAMP_ADDRESSU`.
pub const D3DSAMP_ADDRESSU: u32 = 1;
/// `D3DSAMP_ADDRESSV`.
pub const D3DSAMP_ADDRESSV: u32 = 2;
/// `D3DSAMP_MAGFILTER`.
pub const D3DSAMP_MAGFILTER: u32 = 5;
/// `D3DSAMP_MINFILTER`.
pub const D3DSAMP_MINFILTER: u32 = 6;
/// `D3DSAMP_MIPFILTER` (stored; mip mapping is deferred).
pub const D3DSAMP_MIPFILTER: u32 = 7;

/// `D3DTOP_DISABLE`.
pub const D3DTOP_DISABLE: u32 = 1;
/// `D3DTOP_SELECTARG1`.
pub const D3DTOP_SELECTARG1: u32 = 2;
/// `D3DTOP_SELECTARG2`.
pub const D3DTOP_SELECTARG2: u32 = 3;
/// `D3DTOP_MODULATE`.
pub const D3DTOP_MODULATE: u32 = 4;

/// `D3DTA_DIFFUSE`.
pub const D3DTA_DIFFUSE: u32 = 0;
/// `D3DTA_CURRENT` (stage 0 = the diffuse color).
pub const D3DTA_CURRENT: u32 = 1;
/// `D3DTA_TEXTURE`.
pub const D3DTA_TEXTURE: u32 = 2;

/// `D3DTADDRESS_WRAP`.
pub const D3DTADDRESS_WRAP: u32 = 1;
/// `D3DTADDRESS_CLAMP`.
pub const D3DTADDRESS_CLAMP: u32 = 3;

/// `D3DTEXF_POINT` (nearest).
pub const D3DTEXF_POINT: u32 = 1;
/// `D3DTEXF_LINEAR` (bilinear).
pub const D3DTEXF_LINEAR: u32 = 2;

// ── Blend + depth render-state constants (d3d9types.h values) ──────────

/// `D3DRS_ZENABLE`.
pub const D3DRS_ZENABLE: u32 = 7;
/// `D3DRS_ZWRITEENABLE`.
pub const D3DRS_ZWRITEENABLE: u32 = 14;
/// `D3DRS_SRCBLEND`.
pub const D3DRS_SRCBLEND: u32 = 19;
/// `D3DRS_DESTBLEND`.
pub const D3DRS_DESTBLEND: u32 = 20;
/// `D3DRS_ZFUNC`.
pub const D3DRS_ZFUNC: u32 = 23;
/// `D3DRS_ALPHABLENDENABLE`.
pub const D3DRS_ALPHABLENDENABLE: u32 = 27;
/// `D3DRS_BLENDOP`.
pub const D3DRS_BLENDOP: u32 = 171;

/// `D3DZB_FALSE` (0) — depth test+write disabled.
pub const D3DZB_FALSE: u32 = 0;
/// `D3DZB_TRUE` (1) — depth test+write enabled.
pub const D3DZB_TRUE: u32 = 1;
/// `D3DZB_USEW` (2) — depth write only, test disabled.
pub const D3DZB_USEW: u32 = 2;

/// `D3DCMP_NEVER`.
pub const D3DCMP_NEVER: u32 = 1;
/// `D3DCMP_LESS`.
pub const D3DCMP_LESS: u32 = 2;
/// `D3DCMP_EQUAL`.
pub const D3DCMP_EQUAL: u32 = 3;
/// `D3DCMP_LESSEQUAL`.
pub const D3DCMP_LESSEQUAL: u32 = 4;
/// `D3DCMP_GREATER`.
pub const D3DCMP_GREATER: u32 = 5;
/// `D3DCMP_NOTEQUAL`.
pub const D3DCMP_NOTEQUAL: u32 = 6;
/// `D3DCMP_GREATEREQUAL`.
pub const D3DCMP_GREATEREQUAL: u32 = 7;
/// `D3DCMP_ALWAYS`.
pub const D3DCMP_ALWAYS: u32 = 8;

/// `D3DBLEND_ZERO`.
pub const D3DBLEND_ZERO: u32 = 1;
/// `D3DBLEND_ONE`.
pub const D3DBLEND_ONE: u32 = 2;
/// `D3DBLEND_SRCCOLOR`.
pub const D3DBLEND_SRCCOLOR: u32 = 3;
/// `D3DBLEND_INVSRCCOLOR`.
pub const D3DBLEND_INVSRCCOLOR: u32 = 4;
/// `D3DBLEND_SRCALPHA`.
pub const D3DBLEND_SRCALPHA: u32 = 5;
/// `D3DBLEND_INVSRCALPHA`.
pub const D3DBLEND_INVSRCALPHA: u32 = 6;
/// `D3DBLEND_DESTALPHA`.
pub const D3DBLEND_DESTALPHA: u32 = 7;
/// `D3DBLEND_INVDESTALPHA`.
pub const D3DBLEND_INVDESTALPHA: u32 = 8;
/// `D3DBLEND_DESTCOLOR`.
pub const D3DBLEND_DESTCOLOR: u32 = 9;
/// `D3DBLEND_INVDESTCOLOR`.
pub const D3DBLEND_INVDESTCOLOR: u32 = 10;

/// `D3DBLENDOP_ADD`.
pub const D3DBLENDOP_ADD: u32 = 1;
/// `D3DBLENDOP_SUBTRACT`.
pub const D3DBLENDOP_SUBTRACT: u32 = 2;
/// `D3DBLENDOP_REVSUBTRACT`.
pub const D3DBLENDOP_REVSUBTRACT: u32 = 3;

// ── Typed render-state values (D3DRS_* / D3DTSS_* / D3DSAMP_*) ─────────
//
// The D3DRS_*/D3DTSS_*/D3DSAMP_* values arrive in guest registers and are
// decoded once at the Set* handler boundary, then stored typed. The `Unknown`
// variants preserve raw guest values (the software pipeline treats unknown
// values leniently, as documented on the blend/depth helpers).

/// `D3DZB_*` depth-buffer mode (the `D3DRS_ZENABLE` value).
///
/// The D3D9 values are guest register input and must round-trip (including
/// unknown values), so `Unknown` carries the raw `u32` — that forbids
/// explicit discriminants here; [`Self::from_u32`] / [`Self::as_u32`] are the
/// authoritative D3D9 mapping (locked by the round-trip test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3dZBufferType {
    /// `D3DZB_FALSE` (0) — depth test+write disabled.
    False,
    /// `D3DZB_TRUE` (1) — depth test+write enabled.
    True,
    /// `D3DZB_USEW` (2) — depth write only, test disabled.
    UseW,
    /// Unmodeled value (raw preserved).
    Unknown(u32),
}

impl D3dZBufferType {
    /// Decode a `D3DRS_ZENABLE` value (never fails).
    #[must_use]
    pub const fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::False,
            1 => Self::True,
            2 => Self::UseW,
            _ => Self::Unknown(v),
        }
    }

    /// The raw `D3DZB_*` value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::False => 0,
            Self::True => 1,
            Self::UseW => 2,
            Self::Unknown(v) => v,
        }
    }
}

/// `D3DCMP_*` depth-comparison function (the `D3DRS_ZFUNC` value).
///
/// See [`D3dZBufferType`] for why the discriminants are implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3dCmpFunc {
    /// `D3DCMP_NEVER` (1).
    Never,
    /// `D3DCMP_LESS` (2).
    Less,
    /// `D3DCMP_EQUAL` (3).
    Equal,
    /// `D3DCMP_LESSEQUAL` (4).
    LessEqual,
    /// `D3DCMP_GREATER` (5).
    Greater,
    /// `D3DCMP_NOTEQUAL` (6).
    NotEqual,
    /// `D3DCMP_GREATEREQUAL` (7).
    GreaterEqual,
    /// `D3DCMP_ALWAYS` (8).
    Always,
    /// Unmodeled value (raw preserved).
    Unknown(u32),
}

impl D3dCmpFunc {
    /// Decode a `D3DRS_ZFUNC` value (never fails).
    #[must_use]
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Never,
            2 => Self::Less,
            3 => Self::Equal,
            4 => Self::LessEqual,
            5 => Self::Greater,
            6 => Self::NotEqual,
            7 => Self::GreaterEqual,
            8 => Self::Always,
            _ => Self::Unknown(v),
        }
    }

    /// The raw `D3DCMP_*` value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Never => 1,
            Self::Less => 2,
            Self::Equal => 3,
            Self::LessEqual => 4,
            Self::Greater => 5,
            Self::NotEqual => 6,
            Self::GreaterEqual => 7,
            Self::Always => 8,
            Self::Unknown(v) => v,
        }
    }
}

/// `D3DBLEND_*` blend factor (the `D3DRS_SRCBLEND` / `D3DRS_DESTBLEND` value).
///
/// See [`D3dZBufferType`] for why the discriminants are implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3dBlend {
    /// `D3DBLEND_ZERO` (1).
    Zero,
    /// `D3DBLEND_ONE` (2).
    One,
    /// `D3DBLEND_SRCCOLOR` (3).
    SrcColor,
    /// `D3DBLEND_INVSRCCOLOR` (4).
    InvSrcColor,
    /// `D3DBLEND_SRCALPHA` (5).
    SrcAlpha,
    /// `D3DBLEND_INVSRCALPHA` (6).
    InvSrcAlpha,
    /// `D3DBLEND_DESTALPHA` (7).
    DestAlpha,
    /// `D3DBLEND_INVDESTALPHA` (8).
    InvDestAlpha,
    /// `D3DBLEND_DESTCOLOR` (9).
    DestColor,
    /// `D3DBLEND_INVDESTCOLOR` (10).
    InvDestColor,
    /// Unmodeled value (raw preserved).
    Unknown(u32),
}

impl D3dBlend {
    /// Decode a `D3DBLEND_*` value (never fails).
    #[must_use]
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Zero,
            2 => Self::One,
            3 => Self::SrcColor,
            4 => Self::InvSrcColor,
            5 => Self::SrcAlpha,
            6 => Self::InvSrcAlpha,
            7 => Self::DestAlpha,
            8 => Self::InvDestAlpha,
            9 => Self::DestColor,
            10 => Self::InvDestColor,
            _ => Self::Unknown(v),
        }
    }

    /// The raw `D3DBLEND_*` value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Zero => 1,
            Self::One => 2,
            Self::SrcColor => 3,
            Self::InvSrcColor => 4,
            Self::SrcAlpha => 5,
            Self::InvSrcAlpha => 6,
            Self::DestAlpha => 7,
            Self::InvDestAlpha => 8,
            Self::DestColor => 9,
            Self::InvDestColor => 10,
            Self::Unknown(v) => v,
        }
    }
}

/// `D3DBLENDOP_*` blend operation (the `D3DRS_BLENDOP` value).
///
/// See [`D3dZBufferType`] for why the discriminants are implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3dBlendOp {
    /// `D3DBLENDOP_ADD` (1).
    Add,
    /// `D3DBLENDOP_SUBTRACT` (2).
    Subtract,
    /// `D3DBLENDOP_REVSUBTRACT` (3).
    RevSubtract,
    /// Unmodeled value (raw preserved).
    Unknown(u32),
}

impl D3dBlendOp {
    /// Decode a `D3DBLENDOP_*` value (never fails).
    #[must_use]
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Add,
            2 => Self::Subtract,
            3 => Self::RevSubtract,
            _ => Self::Unknown(v),
        }
    }

    /// The raw `D3DBLENDOP_*` value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Add => 1,
            Self::Subtract => 2,
            Self::RevSubtract => 3,
            Self::Unknown(v) => v,
        }
    }
}

/// Typed device render state (`D3DRS_*`), decoded at the `SetRenderState` /
/// `GetRenderState` register boundary and read by the per-draw fragment stage.
///
/// Unmodeled `D3DRS_*` states are dropped by the handler (they have no effect
/// on the software pipeline and their `GetRenderState` reads fall back to 0,
/// D3D9's default for unused states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderState {
    /// `D3DRS_ALPHABLENDENABLE`.
    pub alpha_blend_enable: bool,
    /// `D3DRS_ZWRITEENABLE`.
    pub z_write_enable: bool,
    /// `D3DRS_ZENABLE` (`D3DZB_*`).
    pub z_enable: D3dZBufferType,
    /// `D3DRS_ZFUNC` (`D3DCMP_*`).
    pub z_func: D3dCmpFunc,
    /// `D3DRS_SRCBLEND` (`D3DBLEND_*`).
    pub src_blend: D3dBlend,
    /// `D3DRS_DESTBLEND` (`D3DBLEND_*`).
    pub dest_blend: D3dBlend,
    /// `D3DRS_BLENDOP` (`D3DBLENDOP_*`).
    pub blend_op: D3dBlendOp,
}

impl Default for RenderState {
    /// D3D9 fixed-function defaults (the old per-state `render_state`
    /// fallback values).
    fn default() -> Self {
        Self {
            alpha_blend_enable: false,
            z_write_enable: true,
            z_enable: D3dZBufferType::False,
            z_func: D3dCmpFunc::LessEqual,
            src_blend: D3dBlend::One,
            dest_blend: D3dBlend::Zero,
            blend_op: D3dBlendOp::Add,
        }
    }
}

/// One texture stage's fixed-function state (stage index is the array slot).
///
/// Merges the two register namespaces: `SetTextureStageState` (`D3DTSS_*`,
/// color-blend state) and `SetSamplerState` (`D3DSAMP_*`, sampling state).
/// Their constants collide numerically (`D3DTSS_COLOROP == D3DSAMP_ADDRESSU
/// == 1`, …), so the per-call decode routes each to its own field. The legacy
/// d3d8 TSS address/filter aliases are gone in D3D9 — address modes and
/// filters live only in the sampler namespace here. Unmodeled slots are
/// preserved verbatim so the `Get*` round-trips still see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureStageState {
    /// `D3DTSS_COLOROP` (default `D3DTOP_MODULATE`).
    pub color_op: u32,
    /// `D3DTSS_COLORARG1` (default `D3DTA_TEXTURE`).
    pub color_arg1: u32,
    /// `D3DTSS_COLORARG2` (default `D3DTA_DIFFUSE`).
    pub color_arg2: u32,
    /// `D3DTSS_ALPHAOP` (default `D3DTOP_MODULATE`).
    pub alpha_op: u32,
    /// `D3DTSS_ALPHAARG1` (default `D3DTA_TEXTURE`).
    pub alpha_arg1: u32,
    /// `D3DTSS_ALPHAARG2` (default `D3DTA_DIFFUSE`).
    pub alpha_arg2: u32,
    /// `D3DTSS_TEXCOORDINDEX`.
    pub tex_coord_index: u32,
    /// `D3DSAMP_ADDRESSU` (default `D3DTADDRESS_WRAP`).
    pub address_u: u32,
    /// `D3DSAMP_ADDRESSV` (default `D3DTADDRESS_WRAP`).
    pub address_v: u32,
    /// `D3DSAMP_MAGFILTER` (default `D3DTEXF_POINT`).
    pub mag_filter: u32,
    /// `D3DSAMP_MINFILTER` (default `D3DTEXF_POINT`).
    pub min_filter: u32,
    /// `D3DSAMP_MIPFILTER` (default `D3DTEXF_POINT`).
    pub mip_filter: u32,
    /// Unmodeled `D3DTSS_*` slots `(slot, value)`, preserved for `Get*`.
    pub other_tss: Vec<(u32, u32)>,
    /// Unmodeled `D3DSAMP_*` slots `(slot, value)`, preserved for `Get*`.
    pub other_sampler: Vec<(u32, u32)>,
}

impl Default for TextureStageState {
    /// D3D9 stage-0 fixed-function defaults (the old `stage_state` fallback
    /// values).
    fn default() -> Self {
        Self {
            color_op: D3DTOP_MODULATE,
            color_arg1: D3DTA_TEXTURE,
            color_arg2: D3DTA_DIFFUSE,
            alpha_op: D3DTOP_MODULATE,
            alpha_arg1: D3DTA_TEXTURE,
            alpha_arg2: D3DTA_DIFFUSE,
            tex_coord_index: 0,
            address_u: D3DTADDRESS_WRAP,
            address_v: D3DTADDRESS_WRAP,
            mag_filter: D3DTEXF_POINT,
            min_filter: D3DTEXF_POINT,
            mip_filter: D3DTEXF_POINT,
            other_tss: Vec::new(),
            other_sampler: Vec::new(),
        }
    }
}

/// D3D9 4×4 matrix: column-major storage, row-vector convention (`v' = v·M`).
pub type Mat4 = [f32; 16];

/// Identity matrix.
pub const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

/// Parsed FVF layout for one vertex stream.
///
/// `#[expect(struct_excessive_bools)]`: the four flags mirror the four D3DFVF
/// bits a stream can carry — folding them into one bitfield would obscure the
/// layout math for no size win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools)]
pub struct FvfLayout {
    /// Bytes consumed by the position (12 for XYZ, 16 for XYZRHW).
    pub position_bytes: u32,
    /// Whether the position is pre-transformed (`D3DFVF_XYZRHW`).
    pub pre_transformed: bool,
    /// `D3DFVF_NORMAL` present.
    pub has_normal: bool,
    /// `D3DFVF_DIFFUSE` present.
    pub has_diffuse: bool,
    /// `D3DFVF_SPECULAR` present.
    pub has_specular: bool,
    /// Number of texture-coordinate sets (`TEX1..TEXn`, 2 floats each).
    pub tex_coords: u32,
    /// Bytes per vertex for this FVF (without any stream padding).
    pub stride: u32,
}

/// Count the number of contiguous texture-coordinate sets (`TEX1..TEXn`).
#[must_use]
fn count_tex_sets(fvf: u32) -> u32 {
    let tex_bits = (fvf >> 8) & 0xFF;
    let mut count = 0_u32;
    for bit in 0..8 {
        if tex_bits & (1 << bit) != 0 {
            count = u32::try_from(bit).unwrap_or(0).saturating_add(1);
        } else {
            break;
        }
    }
    count
}

/// Parse a `D3DFVF` mask into a layout; `None` for unsupported masks
/// (missing or both XYZ/XYZRHW flags — D3D9 requires exactly one).
#[must_use]
pub fn parse_fvf(fvf: u32) -> Option<FvfLayout> {
    let has_xyz = fvf & D3DFVF_XYZ != 0;
    let has_rhw = fvf & D3DFVF_XYZRHW != 0;
    if has_xyz == has_rhw {
        // Slice 1 has no D3DFVF_XYZW / last-beta forms either.
        return None;
    }
    let position_bytes: u32 = if has_rhw { 16 } else { 12 };
    let tex_coords = count_tex_sets(fvf);
    let mut stride = position_bytes;
    if fvf & D3DFVF_NORMAL != 0 {
        stride = stride.saturating_add(12);
    }
    if fvf & D3DFVF_DIFFUSE != 0 {
        stride = stride.saturating_add(4);
    }
    if fvf & D3DFVF_SPECULAR != 0 {
        stride = stride.saturating_add(4);
    }
    stride = stride.saturating_add(tex_coords.saturating_mul(8));
    Some(FvfLayout {
        position_bytes,
        pre_transformed: has_rhw,
        has_normal: fvf & D3DFVF_NORMAL != 0,
        has_diffuse: fvf & D3DFVF_DIFFUSE != 0,
        has_specular: fvf & D3DFVF_SPECULAR != 0,
        tex_coords,
        stride,
    })
}

/// A raw vertex decoded from a guest vertex stream.
#[derive(Debug, Clone, Copy)]
pub struct GuestVertex {
    /// Position X (model/world space for XYZ, screen space for XYZRHW).
    pub x: f32,
    /// Position Y.
    pub y: f32,
    /// Position Z.
    pub z: f32,
    /// Homogeneous W (`XYZRHW` value, or `1.0` for `XYZ`).
    pub w: f32,
    /// Diffuse color `0xAARRGGBB` (opaque white when absent).
    pub color: u32,
    /// First texture-coordinate set U (0.0 when no `D3DFVF_TEX1`).
    pub u: f32,
    /// First texture-coordinate set V (0.0 when no `D3DFVF_TEX1`).
    pub v: f32,
}

/// Read one little-endian `f32` from a byte slice at `offset`.
#[must_use]
fn read_f32(data: &[u8], offset: usize) -> Option<f32> {
    let end = offset.checked_add(4)?;
    let bytes: [u8; 4] = data.get(offset..end)?.try_into().ok()?;
    Some(f32::from_le_bytes(bytes))
}

/// Read one little-endian `u32` from a byte slice at `offset`.
#[must_use]
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes: [u8; 4] = data.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// Decode one vertex at `offset` in a batched vertex buffer (`data` holds the
/// whole stream, `fvf` describes the per-vertex layout).
#[must_use]
#[expect(clippy::many_single_char_names)] // position/uv floats x/y/z/w/u/v
pub fn parse_vertex(data: &[u8], offset: usize, fvf: &FvfLayout) -> Option<GuestVertex> {
    let mut off = offset;
    let x = read_f32(data, off)?;
    off = off.checked_add(4)?;
    let y = read_f32(data, off)?;
    off = off.checked_add(4)?;
    let z = read_f32(data, off)?;
    off = off.checked_add(4)?;
    let w = if fvf.pre_transformed {
        let value = read_f32(data, off)?;
        off = off.checked_add(4)?;
        value
    } else {
        1.0
    };
    if fvf.has_normal {
        off = off.checked_add(12)?;
    }
    let color = if fvf.has_diffuse {
        let value = read_u32(data, off)?;
        off = off.checked_add(4)?;
        value
    } else {
        0xFF_FF_FF_FF
    };
    if fvf.has_specular {
        let _ = off.checked_add(4)?;
    }
    // Texture coordinates: read the first set (u, v) for sampling; skip any
    // further sets (only `D3DFVF_TEX1` is sampled in P4b).
    let (u, v) = if fvf.tex_coords > 0 {
        let u = read_f32(data, off)?;
        off = off.checked_add(4)?;
        let v = read_f32(data, off)?;
        off = off.checked_add(4)?;
        for _ in 1..fvf.tex_coords {
            off = off.checked_add(8)?;
        }
        (u, v)
    } else {
        (0.0, 0.0)
    };
    Some(GuestVertex {
        x,
        y,
        z,
        w,
        color,
        u,
        v,
    })
}

/// Multiply matrices `a` and `b` (row-vector convention: `(a·b)·p == a·(b·p)`).
///
/// `#[expect(arithmetic_side_effects)]`: the index arithmetic is bounded by
/// the 0..4 loop ranges (max `3*4+3 = 15`), so it cannot overflow.
#[must_use]
#[expect(clippy::arithmetic_side_effects)]
pub fn mat4_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0_f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            let mut sum = 0.0_f32;
            for k in 0..4 {
                let a_ik = a.get(i * 4 + k).copied().unwrap_or(0.0);
                let b_kj = b.get(k * 4 + j).copied().unwrap_or(0.0);
                sum += a_ik * b_kj;
            }
            if let Some(slot) = out.get_mut(i * 4 + j) {
                *slot = sum;
            }
        }
    }
    out
}

/// Transform a row vector `v` by matrix `m` (`v' = v·m`).
///
/// `#[expect(arithmetic_side_effects)]`: index arithmetic bounded by 0..4.
#[must_use]
#[expect(clippy::arithmetic_side_effects)]
pub fn transform_point(v: [f32; 4], m: &Mat4) -> [f32; 4] {
    let mut out = [0.0_f32; 4];
    for (j, out_j) in out.iter_mut().enumerate() {
        let mut sum = 0.0_f32;
        for (k, v_k) in v.iter().enumerate() {
            let m_kj = m.get(k * 4 + j).copied().unwrap_or(0.0);
            sum += v_k * m_kj;
        }
        *out_j = sum;
    }
    out
}

/// D3D9 viewport state (`D3DVIEWPORT9`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Top-left X of the viewport in backbuffer pixels.
    pub x: u32,
    /// Top-left Y of the viewport in backbuffer pixels.
    pub y: u32,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Minimum depth (`MinZ`, unused without a z-buffer).
    pub min_z: f32,
    /// Maximum depth (`MaxZ`, unused without a z-buffer).
    pub max_z: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            min_z: 0.0,
            max_z: 1.0,
        }
    }
}

/// Map a clip-space vertex to screen pixels; `None` when the vertex is at or
/// behind the near plane (`w <= 0`) — slice 1 rejects the whole triangle.
///
/// `#[expect(casts)]`: viewport fields are `u32` but the mapping is float
/// math — `f32: From<u32>` does not exist in `std`, and a 32-bit viewport
/// coordinate (≤ ~2^24 px) cannot lose precision in `f32`'s 23-bit mantissa.
#[must_use]
#[expect(clippy::as_conversions, clippy::cast_precision_loss)]
pub fn clip_to_screen(c: [f32; 4], vp: &Viewport) -> Option<(f32, f32)> {
    let [_x, _y, _z, w] = c;
    if w <= 0.0 {
        return None;
    }
    let nx = c[0] / w;
    let ny = c[1] / w;
    let sx = vp.x as f32 + (nx + 1.0) * 0.5 * vp.width as f32;
    let sy = vp.y as f32 + (1.0 - ny) * 0.5 * vp.height as f32;
    Some((sx, sy))
}

/// A screen-space vertex ready for rasterization.
#[derive(Debug, Clone, Copy)]
pub struct ScreenVertex {
    /// Screen X.
    pub x: f32,
    /// Screen Y.
    pub y: f32,
    /// Depth: screen-space 0..1 for `XYZRHW`; the raw model z for `XYZ`
    /// (the software pipeline applies no viewport z transform — affine depth,
    /// consistent with the affine uv interpolation).
    pub z: f32,
    /// Diffuse color `0xAARRGGBB`.
    pub color: u32,
    /// Texture coordinate U (affine-interpolated across the triangle).
    pub u: f32,
    /// Texture coordinate V.
    pub v: f32,
}

/// Resolved stage-0 texture state for one draw: the bound texture's texels
/// plus the sampler/color-op configuration that the fragment stage evaluates.
#[derive(Debug, Clone, Copy)]
pub struct TextureStage<'a> {
    /// Texels in `0xAARRGGBB` (D3DCOLOR) order, row-major, top row first.
    pub pixels: &'a [u32],
    /// Texture width in texels.
    pub width: u32,
    /// Texture height in texels.
    pub height: u32,
    /// U address mode (`D3DTADDRESS_WRAP` / `D3DTADDRESS_CLAMP`).
    pub addr_u: u32,
    /// V address mode.
    pub addr_v: u32,
    /// Sampling filter (`D3DTEXF_POINT` / `D3DTEXF_LINEAR`).
    pub mag_filter: u32,
    /// `D3DTSS_COLOROP` (`D3DTOP_MODULATE` / `SELECTARG1` / `SELECTARG2`).
    pub color_op: u32,
    /// `D3DTSS_COLORARG1` (`D3DTA_TEXTURE` / `D3DTA_DIFFUSE` / `D3DTA_CURRENT`).
    pub color_arg1: u32,
    /// `D3DTSS_COLORARG2`.
    pub color_arg2: u32,
    /// `D3DTSS_ALPHAOP` (default `D3DTOP_MODULATE`).
    pub alpha_op: u32,
    /// `D3DTSS_ALPHAARG1` (default `D3DTA_TEXTURE`).
    pub alpha_arg1: u32,
    /// `D3DTSS_ALPHAARG2` (default `D3DTA_DIFFUSE`).
    pub alpha_arg2: u32,
}

/// Resolved blend + depth state for one draw.
///
/// `depth` is the bound depth buffer (None = no depth surface). The backbuffer
/// stays 0RGB — the fragment alpha feeds the blend factors only.
#[derive(Debug)]
pub struct FragmentState<'a> {
    /// Bound depth buffer texels (None = no depth surface).
    pub depth: Option<&'a mut [f32]>,
    /// `D3DRS_ZENABLE` (0 = off, 1 = test+write, 2 = write only).
    pub z_enable: u32,
    /// `D3DRS_ZFUNC` (`D3DCMP_*`).
    pub z_func: u32,
    /// `D3DRS_ZWRITEENABLE` (bool as u32).
    pub z_write: u32,
    /// `D3DRS_ALPHABLENDENABLE` (bool as u32).
    pub alpha_blend: u32,
    /// `D3DRS_SRCBLEND` (`D3DBLEND_*`).
    pub src_blend: u32,
    /// `D3DRS_DESTBLEND`.
    pub dest_blend: u32,
    /// `D3DRS_BLENDOP` (`D3DBLENDOP_*`).
    pub blend_op: u32,
}

// ── P5a: PS 2.0 interpreter (fragment stage) ───────────────────────────

/// Executable pixel-shader program for one draw: the tokenized instructions
/// plus the constant and sampler state resolved at the draw boundary.
///
/// `constants` is copied per draw (32 float4s); `instructions` and `samplers`
/// borrow the bound shader record and texture records, which outlive the
/// rasterization call.
#[derive(Debug)]
pub struct PsProgram<'a> {
    /// Tokenized instructions (borrowed from the bound shader record).
    pub instructions: &'a [PsInstruction],
    /// Constant registers `c0..c31` (`SetPixelShaderConstantF` + `def`).
    pub constants: [[f32; 4]; PS_CONST_COUNT],
    /// Sampler registers `s0..s3` → the bound texture stage (None = unbound).
    pub samplers: [Option<&'a TextureStage<'a>>; PS_SAMPLER_COUNT],
}

/// Per-fragment interpolated inputs to the pixel shader.
#[derive(Debug, Clone, Copy)]
pub struct PsFragmentInput {
    /// `v0` — interpolated diffuse color (0..1 per channel, RGBA order).
    pub v0: [f32; 4],
    /// `v1` — specular color (unbound in the current FVF pipeline → 0).
    pub v1: [f32; 4],
    /// `t0` — interpolated texture-coordinate set 0 (`u, v, 0, 1`).
    pub t0: [f32; 4],
}

/// The ps_2_0 per-fragment register file.
struct PsRegisters {
    temp: [[f32; 4]; PS_TEMP_COUNT],
    constants: [[f32; 4]; PS_CONST_COUNT],
    input: [[f32; 4]; PS_INPUT_COUNT],
    /// `t0..t3` (only `t0` is interpolated; the rest read `(0,0,0,1)`).
    texcoord: [[f32; 4]; 4],
    /// `oC0` (the fragment color that feeds the blend stage).
    output: [f32; 4],
}

/// Read the `i`-th component without indexing syntax (repo lint).
#[inline]
#[must_use]
fn comp(value: [f32; 4], i: usize) -> f32 {
    value.get(i).copied().unwrap_or(0.0)
}

/// Apply a source modifier (`D3DSPSM_*`) to a register value.
///
/// `DZ`/`DW` (texcoord-depth modifiers) and `NOT` (boolean registers) are
/// unmodeled and read as identity — they do not occur in the ps_2_0 subset
/// this lane executes (documented).
#[must_use]
fn apply_src_mod(value: [f32; 4], src_mod: u8) -> [f32; 4] {
    let [x, y, z, w] = value;
    match src_mod {
        D3DSPSM_NEG => [-x, -y, -z, -w],
        D3DSPSM_BIAS => [x - 0.5, y - 0.5, z - 0.5, w - 0.5],
        D3DSPSM_BIASNEG => [0.5 - x, 0.5 - y, 0.5 - z, 0.5 - w],
        D3DSPSM_SIGN => [
            if x >= 0.0 { 1.0 } else { -1.0 },
            if y >= 0.0 { 1.0 } else { -1.0 },
            if z >= 0.0 { 1.0 } else { -1.0 },
            if w >= 0.0 { 1.0 } else { -1.0 },
        ],
        D3DSPSM_SIGNNEG => [
            if x >= 0.0 { -1.0 } else { 1.0 },
            if y >= 0.0 { -1.0 } else { 1.0 },
            if z >= 0.0 { -1.0 } else { 1.0 },
            if w >= 0.0 { -1.0 } else { 1.0 },
        ],
        D3DSPSM_COMP => [1.0 - x, 1.0 - y, 1.0 - z, 1.0 - w],
        D3DSPSM_X2 => [2.0 * x, 2.0 * y, 2.0 * z, 2.0 * w],
        D3DSPSM_X2NEG => [-2.0 * x, -2.0 * y, -2.0 * z, -2.0 * w],
        D3DSPSM_ABS => [x.abs(), y.abs(), z.abs(), w.abs()],
        D3DSPSM_ABSNEG => [-x.abs(), -y.abs(), -z.abs(), -w.abs()],
        // NONE and any unmodeled modifier pass the value through.
        _ => [x, y, z, w],
    }
}

/// Reorder a register value by the operand's per-component swizzle.
#[must_use]
fn apply_swizzle(value: [f32; 4], swizzle: [u8; 4]) -> [f32; 4] {
    [
        comp(value, usize::from(swizzle[0])),
        comp(value, usize::from(swizzle[1])),
        comp(value, usize::from(swizzle[2])),
        comp(value, usize::from(swizzle[3])),
    ]
}

/// Read a source operand: register fetch → source modifier → swizzle.
#[must_use]
fn read_operand(regs: &PsRegisters, op: &Operand) -> [f32; 4] {
    let base = match op.reg_type {
        RegType::Temp => regs
            .temp
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Const => regs
            .constants
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Input => regs
            .input
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Texture => regs
            .texcoord
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        // ColorOut / Sampler / Other as a source is not valid ps_2_0.
        _ => [0.0; 4],
    };
    apply_swizzle(apply_src_mod(base, op.src_mod), op.swizzle)
}

/// Write an operand's value into the register file (write mask + saturate).
///
/// `_sat` clamps the written components to `[0, 1]`; `_pp` (partial
/// precision) is ignored (full f32 precision, documented). `oDepth` writes
/// are stored nowhere this lane — the fragment depth is still the interpolated
/// z (documented; deferred with the vertex stage).
fn write_operand(regs: &mut PsRegisters, op: &Operand, value: [f32; 4]) {
    let mut result = value;
    if op.dst_mod == D3DSPDM_SATURATE {
        for channel in &mut result {
            *channel = channel.clamp(0.0, 1.0);
        }
    }
    let target = match op.reg_type {
        RegType::Temp => regs.temp.get_mut(usize::from(op.reg_num)),
        RegType::ColorOut => Some(&mut regs.output),
        _ => None, // oDepth etc. not applied this lane
    };
    let Some(target) = target else { return };
    let [x, y, z, w] = result;
    let [ox, oy, oz, ow] = *target;
    *target = [
        if op.write_mask & 0x1 != 0 { x } else { ox },
        if op.write_mask & 0x2 != 0 { y } else { oy },
        if op.write_mask & 0x4 != 0 { z } else { oz },
        if op.write_mask & 0x8 != 0 { w } else { ow },
    ];
}

/// Convert a `0xAARRGGBB` texel to the shader's `[r, g, b, a]` 0..1 float4.
#[must_use]
fn texel_to_float4(texel: u32) -> [f32; 4] {
    [
        f32::from(u8::try_from((texel >> 16) & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from((texel >> 8) & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from(texel & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from((texel >> 24) & 0xFF).unwrap_or(0)) / 255.0,
    ]
}

/// Convert a `0xAARRGGBB` diffuse color to the shader's `[r, g, b, a]` float4.
#[must_use]
fn color_to_float4(color: u32) -> [f32; 4] {
    texel_to_float4(color)
}

/// Execute a pixel shader for one fragment; `None` = `texkill` discarded it.
///
/// Semantics follow the ps_2_0 spec (op operand order: `slt dst, a, b` is
/// `(a < b)`, `sge` is `(a >= b)`, `lrp dst, a, b, c` is `a*b + (1-a)*c`,
/// `cmp dst, a, b, c` is `(a >= 0) ? b : c`; `rcp`/`rsq`/`dp3`/`dp4` replicate
/// their scalar result to all four channels; `exp`/`log` compute only the x
/// channel and copy yzw). Domain edges use plain IEEE f32 arithmetic, which
/// matches the hardware contract: `rcp(0) = +inf`, `rsq(0) = +inf`,
/// `rsq(x<0) = 1/sqrt(|x|)`, `log2(0) = -inf`, `log2(x<0) = NaN`.
#[must_use]
pub fn run_pixel_shader(program: &PsProgram<'_>, input: &PsFragmentInput) -> Option<[f32; 4]> {
    let mut regs = PsRegisters {
        temp: [[0.0; 4]; PS_TEMP_COUNT],
        constants: program.constants,
        input: [input.v0, input.v1],
        texcoord: [
            input.t0,
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        output: [0.0; 4],
    };

    for instr in program.instructions {
        match instr.op {
            PsOp::End => break,
            PsOp::Nop | PsOp::Def | PsOp::Dcl => {}
            PsOp::Mov => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let value = read_operand(&regs, src);
                    write_operand(&mut regs, dst, value);
                }
            }
            PsOp::Add => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]],
                    );
                }
            }
            PsOp::Sub => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]],
                    );
                }
            }
            PsOp::Mad => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0] * b[0] + c[0],
                            a[1] * b[1] + c[1],
                            a[2] * b[2] + c[2],
                            a[3] * b[3] + c[3],
                        ],
                    );
                }
            }
            PsOp::Mul => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] * b[0], a[1] * b[1], a[2] * b[2], a[3] * b[3]],
                    );
                }
            }
            PsOp::Rcp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0);
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
            }
            PsOp::Rsq => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0).abs().sqrt();
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
            }
            PsOp::Dp3 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
            }
            PsOp::Dp4 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
            }
            PsOp::Min => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0].min(b[0]),
                            a[1].min(b[1]),
                            a[2].min(b[2]),
                            a[3].min(b[3]),
                        ],
                    );
                }
            }
            PsOp::Max => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0].max(b[0]),
                            a[1].max(b[1]),
                            a[2].max(b[2]),
                            a[3].max(b[3]),
                        ],
                    );
                }
            }
            PsOp::Slt => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] < b[0] { 1.0 } else { 0.0 },
                            if a[1] < b[1] { 1.0 } else { 0.0 },
                            if a[2] < b[2] { 1.0 } else { 0.0 },
                            if a[3] < b[3] { 1.0 } else { 0.0 },
                        ],
                    );
                }
            }
            PsOp::Sge => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] >= b[0] { 1.0 } else { 0.0 },
                            if a[1] >= b[1] { 1.0 } else { 0.0 },
                            if a[2] >= b[2] { 1.0 } else { 0.0 },
                            if a[3] >= b[3] { 1.0 } else { 0.0 },
                        ],
                    );
                }
            }
            PsOp::Exp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [2.0_f32.powf(sx), sy, sz, sw]);
                }
            }
            PsOp::Log => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [sx.log2(), sy, sz, sw]);
                }
            }
            PsOp::Lrp => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0] * b[0] + (1.0 - a[0]) * c[0],
                            a[1] * b[1] + (1.0 - a[1]) * c[1],
                            a[2] * b[2] + (1.0 - a[2]) * c[2],
                            a[3] * b[3] + (1.0 - a[3]) * c[3],
                        ],
                    );
                }
            }
            PsOp::Frc => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [x, y, z, w] = read_operand(&regs, src);
                    write_operand(
                        &mut regs,
                        dst,
                        [x - x.floor(), y - y.floor(), z - z.floor(), w - w.floor()],
                    );
                }
            }
            PsOp::Cmp => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] >= 0.0 { b[0] } else { c[0] },
                            if a[1] >= 0.0 { b[1] } else { c[1] },
                            if a[2] >= 0.0 { b[2] } else { c[2] },
                            if a[3] >= 0.0 { b[3] } else { c[3] },
                        ],
                    );
                }
            }
            PsOp::Tex => {
                // texld rD, tN, sM — sample sampler sM at (tN.x, tN.y).
                if let Some(dst) = &instr.dst
                    && let Some(uv_src) = instr.srcs.first()
                {
                    let [u, v, _, _] = read_operand(&regs, uv_src);
                    let texel = instr
                        .srcs
                        .get(1)
                        .and_then(|sampler| program.samplers.get(usize::from(sampler.reg_num)))
                        .and_then(|stage| *stage)
                        .map_or(0, |stage| sample_texture(stage, u, v));
                    write_operand(&mut regs, dst, texel_to_float4(texel));
                }
            }
            PsOp::TexKill => {
                if let Some(src) = instr.srcs.first() {
                    let [x, y, z, w] = read_operand(&regs, src);
                    if x < 0.0 || y < 0.0 || z < 0.0 || w < 0.0 {
                        return None;
                    }
                }
            }
            PsOp::Unsupported(_) => return None, // unreachable: Create rejects these
        }
    }
    Some(regs.output)
}

/// Read exactly two source operands; short source lists read as zero.
#[must_use]
fn read_two(regs: &PsRegisters, srcs: &[Operand]) -> [[f32; 4]; 2] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b]
}

/// Read exactly three source operands; short source lists read as zero.
#[must_use]
fn read_three(regs: &PsRegisters, srcs: &[Operand]) -> [[f32; 4]; 3] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    let c = srcs.get(2).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b, c]
}

/// Convert the shader's `oC0` float4 to an `0RGB` backbuffer color
/// (`round(v * 255)` per channel, clamped to 0..255).
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // float→u8 narrowing: clamped to [0,255] before the cast
pub fn pixel_shader_color_to_0rgb(oc0: [f32; 4]) -> u32 {
    let channel = |v: f32| {
        let rounded = (v * 255.0).round().clamp(0.0, 255.0);
        rounded as u32
    };
    (channel(comp(oc0, 0)) << 16) | (channel(comp(oc0, 1)) << 8) | channel(comp(oc0, 2))
}

/// Convert the shader's `oC0` alpha (0..1) to the blend-stage alpha byte.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // float→u8 narrowing: clamped to [0,255] before the cast
pub fn pixel_shader_alpha_to_u8(oc0: [f32; 4]) -> u8 {
    let a = (comp(oc0, 3) * 255.0).round().clamp(0.0, 255.0);
    a as u8
}

/// Resolve a texel index from a raw (texel-space) index with the address mode.
///
/// `WRAP` mirrors the coordinate modulo the size; `CLAMP` (and any unknown
/// mode) saturates into `[0, size-1]`. `size` is at least 1.
#[must_use]
#[expect(clippy::arithmetic_side_effects)] // mod/saturate on a bounded index
fn address_index(index: i32, size: u32, address: u32) -> i32 {
    let size_i = i32::try_from(size).unwrap_or(0).max(1);
    if address == D3DTADDRESS_WRAP {
        ((index % size_i) + size_i) % size_i
    } else {
        index.max(0).min(size_i - 1)
    }
}

/// Map a normalized texture coordinate to a texel index for point sampling.
///
/// `floor(u * size)` so the last texel (`u` just under 1.0) is reachable.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn point_index(coord: f32, size: u32, address: u32) -> i32 {
    let size_f = size as f32;
    let raw = (coord * size_f.max(1.0)).floor() as i32;
    address_index(raw, size, address)
}

/// Fetch the texel at integer indices (addressed).
fn texel_at(stage: &TextureStage<'_>, x: i32, y: i32) -> u32 {
    let x = address_index(x, stage.width, stage.addr_u);
    let y = address_index(y, stage.height, stage.addr_v);
    let index = usize::try_from(y)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(stage.width).unwrap_or(0))
        .saturating_add(usize::try_from(x).unwrap_or(0));
    stage.pixels.get(index).copied().unwrap_or(0)
}

/// Sample one texel (point or bilinear) at a normalized `(u, v)`.
///
/// `#[expect(casts)]`: the bilinear weights are float math over `u8` channels
/// — `std` has no lossless float↔int `From`; results are clamped to 0..255.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names
)]
fn sample_texture(stage: &TextureStage<'_>, u: f32, v: f32) -> u32 {
    if stage.mag_filter == D3DTEXF_LINEAR {
        let x = u * stage.width as f32 - 0.5;
        let y = v * stage.height as f32 - 0.5;
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let t00 = texel_at(stage, x0, y0);
        let t10 = texel_at(stage, x0 + 1, y0);
        let t01 = texel_at(stage, x0, y0 + 1);
        let t11 = texel_at(stage, x0 + 1, y0 + 1);
        let lerp_channel = |a: u8, b: u8, f: f32| {
            let a = f32::from(a);
            let b = f32::from(b);
            (a + (b - a) * f).round() as u8
        };
        let top = [
            lerp_channel(
                u8::try_from((t00 >> 16) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 16) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t00 >> 8) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 8) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from(t00 & 0xFF).unwrap_or(0),
                u8::try_from(t10 & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t00 >> 24) & 0xFF).unwrap_or(0),
                u8::try_from((t10 >> 24) & 0xFF).unwrap_or(0),
                fx,
            ),
        ];
        let bottom = [
            lerp_channel(
                u8::try_from((t01 >> 16) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 16) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t01 >> 8) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 8) & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from(t01 & 0xFF).unwrap_or(0),
                u8::try_from(t11 & 0xFF).unwrap_or(0),
                fx,
            ),
            lerp_channel(
                u8::try_from((t01 >> 24) & 0xFF).unwrap_or(0),
                u8::try_from((t11 >> 24) & 0xFF).unwrap_or(0),
                fx,
            ),
        ];
        let mix = |a: u8, b: u8| {
            let a = f32::from(a);
            let b = f32::from(b);
            (a + (b - a) * fy).round() as u8
        };
        let r = mix(top[0], bottom[0]);
        let g = mix(top[1], bottom[1]);
        let b = mix(top[2], bottom[2]);
        let a = mix(top[3], bottom[3]);
        (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
    } else {
        let x = point_index(u, stage.width, stage.addr_u);
        let y = point_index(v, stage.height, stage.addr_v);
        texel_at(stage, x, y)
    }
}

/// Resolve a `D3DTA_*` color argument to an `0xAARRGGBB` value.
#[must_use]
fn stage_arg(arg: u32, texel: u32, diffuse: u32) -> u32 {
    match arg {
        D3DTA_TEXTURE => texel,
        // Stage 0 has no previous-stage result, so CURRENT is the diffuse.
        D3DTA_DIFFUSE | D3DTA_CURRENT => diffuse,
        _ => 0,
    }
}

/// Evaluate a `D3DTSS_COLOROP` over two `0xAARRGGBB` args → 0RGB.
///
/// `MODULATE` multiplies the RGB channels (alpha byte dropped); unknown ops
/// fall back to `SELECTARG1` (documented — the common subset policy).
#[must_use]
fn eval_color_op(op: u32, arg1: u32, arg2: u32) -> u32 {
    match op {
        D3DTOP_SELECTARG2 => arg2 & 0x00FF_FFFF,
        D3DTOP_MODULATE => {
            let r = (u32::from(u8::try_from((arg1 >> 16) & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from((arg2 >> 16) & 0xFF).unwrap_or(0)))
                >> 8;
            let g = (u32::from(u8::try_from((arg1 >> 8) & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from((arg2 >> 8) & 0xFF).unwrap_or(0)))
                >> 8;
            let b = (u32::from(u8::try_from(arg1 & 0xFF).unwrap_or(0)))
                .saturating_mul(u32::from(u8::try_from(arg2 & 0xFF).unwrap_or(0)))
                >> 8;
            (r << 16) | (g << 8) | b
        }
        _ => arg1 & 0x00FF_FFFF,
    }
}

/// Evaluate a `D3DTSS_ALPHAOP` over two `0xAARRGGBB` args → the alpha byte.
///
/// `MODULATE` multiplies the alpha channels; unknown ops fall back to
/// `SELECTARG1` (documented — the common subset policy).
#[must_use]
fn eval_alpha_op(op: u32, arg1: u32, arg2: u32) -> u8 {
    let a1 = u8::try_from((arg1 >> 24) & 0xFF).unwrap_or(0);
    let a2 = u8::try_from((arg2 >> 24) & 0xFF).unwrap_or(0);
    match op {
        D3DTOP_SELECTARG2 => a2,
        D3DTOP_MODULATE => {
            u8::try_from(u32::from(a1).saturating_mul(u32::from(a2)) >> 8).unwrap_or(0)
        }
        // SELECTARG1 and unknown ops both yield arg1.
        _ => a1,
    }
}

/// Depth compare against `D3DRS_ZFUNC`.
///
/// `#[expect(float_cmp)]`: the exact `==`/`!=` compares are the D3D9 EQUAL /
/// NOTEQUAL semantics on the unquantized software depth values.
#[must_use]
#[expect(clippy::float_cmp)]
fn depth_test(z: f32, existing: f32, func: u32) -> bool {
    match func {
        D3DCMP_NEVER => false,
        D3DCMP_LESS => z < existing,
        D3DCMP_EQUAL => z == existing,
        D3DCMP_LESSEQUAL => z <= existing,
        D3DCMP_GREATER => z > existing,
        D3DCMP_NOTEQUAL => z != existing,
        D3DCMP_GREATEREQUAL => z >= existing,
        // D3DCMP_ALWAYS and unknown funcs always pass.
        _ => true,
    }
}

/// Resolve a `D3DBLEND_*` factor to its `0..=255` fixed-point fraction.
///
/// `DESTALPHA` reads the destination alpha — the backbuffer is 0RGB so that
/// is always 0 (documented); `INVDESTALPHA` is therefore always 1. Unknown
/// factors fall back to `D3DBLEND_ONE` (documented).
#[must_use]
fn blend_factor(factor: u32, src_channel: u32, dest_channel: u32, src_alpha: u32) -> u32 {
    match factor {
        D3DBLEND_ZERO | D3DBLEND_DESTALPHA => 0,
        D3DBLEND_SRCCOLOR => src_channel,
        D3DBLEND_INVSRCCOLOR => 255_u32.saturating_sub(src_channel),
        D3DBLEND_SRCALPHA => src_alpha,
        D3DBLEND_INVSRCALPHA => 255_u32.saturating_sub(src_alpha),
        D3DBLEND_DESTCOLOR => dest_channel,
        D3DBLEND_INVDESTCOLOR => 255_u32.saturating_sub(dest_channel),
        // D3DBLEND_ONE, D3DBLEND_INVDESTALPHA, and unknown factors all yield 255.
        _ => 255,
    }
}

/// Apply one `D3DBLENDOP_*` to a channel: `src*sf` op `dst*df`, `>>8`, clamped
/// to 0..255. `SUBTRACT`/`REVSUBTRACT` clamp at zero; unknown ops → ADD.
#[must_use]
fn blend_channel(
    op: u32,
    src_channel: u32,
    dest_channel: u32,
    src_factor: u32,
    dest_factor: u32,
) -> u32 {
    let src_part = src_channel.saturating_mul(src_factor);
    let dest_part = dest_channel.saturating_mul(dest_factor);
    let raw = match op {
        D3DBLENDOP_SUBTRACT => src_part.saturating_sub(dest_part),
        D3DBLENDOP_REVSUBTRACT => dest_part.saturating_sub(src_part),
        // D3DBLENDOP_ADD and unknown ops.
        _ => src_part.saturating_add(dest_part),
    };
    (raw >> 8).min(255)
}

/// Blend a fragment color over the existing backbuffer pixel.
///
/// `src` is the 0RGB fragment color, `alpha` its alpha byte (the D3D9
/// non-premultiplied convention: out = src*srcFactor op dst*dstFactor).
/// The result is 0RGB — the backbuffer alpha stays 0.
#[must_use]
fn blend_fragment(dst: u32, src: u32, alpha: u8, frag: &FragmentState<'_>) -> u32 {
    let src_alpha = u32::from(alpha);
    let (sr, sg, sb) = ((src >> 16) & 0xFF, (src >> 8) & 0xFF, src & 0xFF);
    let (dr, dg, db) = ((dst >> 16) & 0xFF, (dst >> 8) & 0xFF, dst & 0xFF);
    let channel = |src_ch: u32, dst_ch: u32| {
        let sf = blend_factor(frag.src_blend, src_ch, dst_ch, src_alpha);
        let df = blend_factor(frag.dest_blend, src_ch, dst_ch, src_alpha);
        blend_channel(frag.blend_op, src_ch, dst_ch, sf, df)
    };
    let r = channel(sr, dr);
    let g = channel(sg, dg);
    let b = channel(sb, db);
    (r << 16) | (g << 8) | b
}

/// Top-left rule: whether a pixel exactly on an edge (`e == 0`) counts as
/// inside.
///
/// With the winding normalized so the interior lies on the left of every
/// directed edge, an edge is "top or left" when it points right (non-vertical)
/// or straight up (vertical). Shared edges between adjacent triangles then
/// have single ownership — each is drawn by exactly one triangle, so there
/// are no cracks and no double-draws.
#[must_use]
pub fn is_top_or_left_edge(dx: f32, dy: f32) -> bool {
    if dx == 0.0 { dy < 0.0 } else { dx > 0.0 }
}

#[must_use]
fn edge_inside(e: f32, dx: f32, dy: f32) -> bool {
    // Exact `== 0.0` is deliberate: the top-left rule only breaks ties for
    // pixels exactly on an edge (clippy's float_cmp exempts 0.0 literals).
    e > 0.0 || (e == 0.0 && is_top_or_left_edge(dx, dy))
}

/// Blend three `0xAARRGGBB` colors by barycentric weights → `0xAARRGGBB`
/// (the alpha feeds the P4c blend factors; the backbuffer masks it).
///
/// `#[expect(casts)]`: channel bytes widen to `f32` for the weighted sum and
/// the result narrows back — `std` has no lossless float↔int `From`. The
/// `.round()` results are clamped to 0..255 before narrowing, so truncation
/// and sign-loss are impossible.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn blend_colors(wa: f32, ca: u32, wb: f32, cb: u32, wc: f32, cc: u32) -> u32 {
    let ra = f32::from(u8::try_from((ca >> 16) & 0xFF).unwrap_or(0));
    let ga = f32::from(u8::try_from((ca >> 8) & 0xFF).unwrap_or(0));
    let ba = f32::from(u8::try_from(ca & 0xFF).unwrap_or(0));
    let aa = f32::from(u8::try_from((ca >> 24) & 0xFF).unwrap_or(0));
    let rb = f32::from(u8::try_from((cb >> 16) & 0xFF).unwrap_or(0));
    let gb = f32::from(u8::try_from((cb >> 8) & 0xFF).unwrap_or(0));
    let bb = f32::from(u8::try_from(cb & 0xFF).unwrap_or(0));
    let ab = f32::from(u8::try_from((cb >> 24) & 0xFF).unwrap_or(0));
    let rc = f32::from(u8::try_from((cc >> 16) & 0xFF).unwrap_or(0));
    let gc = f32::from(u8::try_from((cc >> 8) & 0xFF).unwrap_or(0));
    let bc = f32::from(u8::try_from(cc & 0xFF).unwrap_or(0));
    let ac = f32::from(u8::try_from((cc >> 24) & 0xFF).unwrap_or(0));
    let r = (wa * ra + wb * rb + wc * rc).round() as u32;
    let g = (wa * ga + wb * gb + wc * gc).round() as u32;
    let b = (wa * ba + wb * bb + wc * bc).round() as u32;
    let a = (wa * aa + wb * ab + wc * ac).round() as u32;
    (a.min(255) << 24) | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

/// Fill one triangle into `backbuffer` (0RGB, top-down, `width` × `height`)
/// with barycentric solid-fill, accumulating the covered region into `dirty`.
///
/// When `tex` is present and its `color_op` is not `D3DTOP_DISABLE`, each
/// fragment is textured: the uv is affine-interpolated (perspective-correct
/// interpolation deferred — fine for screen-aligned quads), sampled with the
/// stage's address modes/filter, and combined with the Gouraud diffuse via the
/// color op; the alpha op (default MODULATE) yields the fragment alpha.
///
/// When `ps` is present (a pixel shader is bound), the fragment stage runs the
/// PS 2.0 interpreter instead of the FFP color/alpha ops: `oC0` feeds the
/// blend stage exactly where the FFP result would. `texkill` discards the
/// fragment (no color write, no depth write). Blend + depth from P4c apply
/// identically after the shader output is resolved.
///
/// `frag` carries the blend + depth configuration: the depth is affine-
/// interpolated and tested/written per `D3DRS_ZFUNC`/`ZWRITEENABLE` before the
/// color write; then, when `D3DRS_ALPHABLENDENABLE` is set, the fragment color
/// blends over the existing backbuffer pixel with the `D3DBLEND_*` factors.
/// The backbuffer stays 0RGB — alpha feeds the blend only.
///
/// Degenerate (zero-area) triangles and triangles fully off-screen are
/// dropped. The dirty region is the triangle's clipped bounding box —
/// conservative but always a superset of the written pixels.
///
/// `#[expect(casts)]`: pixel coordinates are `i32`/`u32` but the edge
/// functions run in float — `std` has no lossless int↔float `From`, and the
/// bounds are clamped to the backbuffer before any narrowing.
#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]
pub fn rasterize_triangle(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    a: ScreenVertex,
    b: ScreenVertex,
    c: ScreenVertex,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    // Signed area; zero = degenerate (collinear or zero-size triangle).
    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if area == 0.0 {
        return;
    }
    // Normalize the winding so the interior is on the left of every directed
    // edge (`e >= 0` for all three), then apply the top-left boundary rule.
    let (a, b, c) = if area < 0.0 { (a, c, b) } else { (a, b, c) };

    let min_x = a.x.min(b.x).min(c.x).floor() as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil() as i32;
    let min_y = a.y.min(b.y).min(c.y).floor() as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil() as i32;
    let w_i = i32::try_from(width).unwrap_or(i32::MAX);
    let h_i = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = min_x.max(0);
    let y0 = min_y.max(0);
    let x1 = max_x.min(w_i);
    let y1 = max_y.min(h_i);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // D3DZB_USEW (2): write-only depth — test disabled, writes enabled.
    let depth_testing = frag.z_enable != 0 && frag.z_enable != D3DZB_USEW;
    let depth_writing = frag.z_enable != 0 && frag.z_write != 0;
    let blending = frag.alpha_blend != 0;
    let width_us = usize::try_from(width).unwrap_or(0);
    // P5b NEON fast paths (byte-identical; the D3D9_RESTING_FRAME_HASH gate
    // proves gui_d3d9's flat/blended quads hit them without changing pixels):
    // - `fast_blend`: SRCALPHA/INVSRCALPHA/ADD → `blend_0rgb_4x` over runs of
    //   4 contiguous accepted pixels.
    // - `flat_fill`: no blend, no texture, no PS, and all three vertex colors
    //   equal → the Gouraud result is provably the vertex color (weight sum
    //   error ≪ 0.5), so `fill_0rgb_4x` writes the constant color.
    let fast_blend = blending
        && frag.src_blend == D3DBLEND_SRCALPHA
        && frag.dest_blend == D3DBLEND_INVSRCALPHA
        && frag.blend_op == D3DBLENDOP_ADD;
    let flat_fill =
        !blending && tex.is_none() && ps.is_none() && a.color == b.color && b.color == c.color;
    let flat_color = a.color & 0x00FF_FFFF;
    let flat_alpha = u8::try_from((a.color >> 24) & 0xFF).unwrap_or(0);
    for py in y0..y1 {
        // P5b pending batch: up to 4 contiguous accepted pixels (index,
        // fragment color, alpha) flushed by the NEON kernel or the scalar
        // fallback. Reset per row — a batch never crosses rows.
        let mut batch_idx = [0usize; 4];
        let mut batch_rgb = [0u32; 4];
        let mut batch_alpha = [0u8; 4];
        let mut batch_n = 0usize;
        for px in x0..x1 {
            let cx = px as f32 + 0.5;
            let cy = py as f32 + 0.5;
            let e_ab = (b.x - a.x) * (cy - a.y) - (b.y - a.y) * (cx - a.x);
            let e_bc = (c.x - b.x) * (cy - b.y) - (c.y - b.y) * (cx - b.x);
            let e_ca = (a.x - c.x) * (cy - c.y) - (a.y - c.y) * (cx - c.x);
            let inside = edge_inside(e_ab, b.x - a.x, b.y - a.y)
                && edge_inside(e_bc, c.x - b.x, c.y - b.y)
                && edge_inside(e_ca, a.x - c.x, a.y - c.y);
            if !inside {
                continue;
            }
            let sum = e_ab + e_bc + e_ca;
            if sum == 0.0 {
                continue;
            }
            let wa = e_bc / sum;
            let wb = e_ca / sum;
            let wc = e_ab / sum;
            let index = usize::try_from(py)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(px).unwrap_or(0));

            // Depth test + write (before the color write, per pixel).
            let z = wa * a.z + wb * b.z + wc * c.z;
            if let Some(depth) = frag.depth.as_deref_mut() {
                let existing = depth.get(index).copied().unwrap_or(1.0);
                if depth_testing && !depth_test(z, existing, frag.z_func) {
                    continue; // discarded: no color write, no z write
                }
                if depth_writing && let Some(slot) = depth.get_mut(index) {
                    *slot = z;
                }
            }

            let (rgb, alpha) = if flat_fill {
                (flat_color, flat_alpha)
            } else {
                let gcolor = blend_colors(wa, a.color, wb, b.color, wc, c.color);
                let u = wa * a.u + wb * b.u + wc * c.u;
                let v = wa * a.v + wb * b.v + wc * c.v;
                match ps {
                    Some(program) => {
                        // Pixel shader path: v0 = interpolated diffuse, t0 = the
                        // affine-interpolated texture coordinate set 0. The
                        // shader's oC0 feeds the blend stage like the FFP result.
                        let input = PsFragmentInput {
                            v0: color_to_float4(gcolor),
                            v1: [0.0; 4],
                            t0: [u, v, 0.0, 1.0],
                        };
                        match run_pixel_shader(program, &input) {
                            Some(oc0) => (
                                pixel_shader_color_to_0rgb(oc0),
                                pixel_shader_alpha_to_u8(oc0),
                            ),
                            // texkill discarded the fragment: no color, no depth.
                            None => continue,
                        }
                    }
                    None => match tex {
                        Some(stage) if stage.color_op != D3DTOP_DISABLE => {
                            let texel = sample_texture(stage, u, v);
                            let arg1 = stage_arg(stage.color_arg1, texel, gcolor);
                            let arg2 = stage_arg(stage.color_arg2, texel, gcolor);
                            let rgb = eval_color_op(stage.color_op, arg1, arg2);
                            let alpha_arg1 = stage_arg(stage.alpha_arg1, texel, gcolor);
                            let alpha_arg2 = stage_arg(stage.alpha_arg2, texel, gcolor);
                            let alpha = eval_alpha_op(stage.alpha_op, alpha_arg1, alpha_arg2);
                            (rgb, alpha)
                        }
                        _ => (
                            gcolor & 0x00FF_FFFF,
                            u8::try_from((gcolor >> 24) & 0xFF).unwrap_or(0),
                        ),
                    },
                }
            };

            if fast_blend || flat_fill {
                // Batch: the kernel needs 4 CONTIGUOUS accepted pixels, so a
                // rejected pixel (a `continue` above) or a row end breaks the
                // run — the next accepted pixel's non-contiguous index
                // flushes the pending batch via the scalar fallback.
                let prev = batch_idx.get(batch_n.wrapping_sub(1)).copied().unwrap_or(0);
                if batch_n == 4 || (batch_n > 0 && index != prev.saturating_add(1)) {
                    flush_pixel_batch(
                        backbuffer,
                        &batch_rgb,
                        batch_alpha,
                        &batch_idx,
                        batch_n,
                        fast_blend,
                        flat_fill,
                        flat_color,
                        blending,
                        frag,
                    );
                    batch_n = 0;
                }
                if let Some(slot) = batch_idx.get_mut(batch_n) {
                    *slot = index;
                }
                if let Some(slot) = batch_rgb.get_mut(batch_n) {
                    *slot = rgb;
                }
                if let Some(slot) = batch_alpha.get_mut(batch_n) {
                    *slot = alpha;
                }
                batch_n = batch_n.saturating_add(1);
            } else {
                let color = if blending {
                    let dst = backbuffer.get(index).copied().unwrap_or(0);
                    blend_fragment(dst, rgb, alpha, frag)
                } else {
                    rgb
                };
                if let Some(pixel) = backbuffer.get_mut(index) {
                    *pixel = color;
                }
            }
        }
        // Row end: flush the pending batch (partial run → scalar fallback).
        flush_pixel_batch(
            backbuffer,
            &batch_rgb,
            batch_alpha,
            &batch_idx,
            batch_n,
            fast_blend,
            flat_fill,
            flat_color,
            blending,
            frag,
        );
    }

    let rect = IRect {
        left: x0,
        top: y0,
        right: x1,
        bottom: y1,
    };
    // Conservative dirty region: the clipped bounding box, unioned into the
    // accumulated region (a full-dirty frame stays full).
    *dirty = (*dirty).map(|prev| IRect {
        left: prev.left.min(rect.left),
        top: prev.top.min(rect.top),
        right: prev.right.max(rect.right),
        bottom: prev.bottom.max(rect.bottom),
    });
}

/// P5b: flush a pending batch of up to 4 accepted pixels.
///
/// Byte-identical to the scalar per-pixel write path: only when the batch
/// holds 4 CONTIGUOUS pixels and a NEON fast path applies (`fast_blend` =
/// SRCALPHA/INVSRCALPHA/ADD, or `flat_fill` = constant color, no blend) does
/// the kernel write all four at once; otherwise each pixel takes the scalar
/// path (`blend_fragment` when blending, plain write otherwise).
#[allow(clippy::too_many_arguments)]
fn flush_pixel_batch(
    backbuffer: &mut [u32],
    rgb: &[u32; 4],
    alpha: [u8; 4],
    indices: &[usize; 4],
    n: usize,
    fast_blend: bool,
    flat_fill: bool,
    flat_color: u32,
    blending: bool,
    frag: &FragmentState<'_>,
) {
    if n == 0 {
        return;
    }
    let base = indices.first().copied().unwrap_or(0);
    let contiguous = n == 4
        && indices.get(1).copied().unwrap_or(0) == base.saturating_add(1)
        && indices.get(2).copied().unwrap_or(0) == base.saturating_add(2)
        && indices.get(3).copied().unwrap_or(0) == base.saturating_add(3);
    if contiguous && fast_blend {
        // Gather the 4 destination pixels, blend with the NEON kernel,
        // scatter back. `blend_0rgb_4x` is byte-identical to
        // `blend_fragment` for these factors (verified by unit test + the
        // D3D9 resting-frame hash gate).
        let mut dst = [0u32; 4];
        for (i, slot) in dst.iter_mut().enumerate() {
            *slot = backbuffer.get(base.saturating_add(i)).copied().unwrap_or(0);
        }
        blend_0rgb_4x(&mut dst, rgb, &alpha);
        for (i, px) in dst.iter().enumerate() {
            if let Some(pixel) = backbuffer.get_mut(base.saturating_add(i)) {
                *pixel = *px;
            }
        }
        return;
    }
    if contiguous && flat_fill {
        if let Some(span) = backbuffer.get_mut(base..base.saturating_add(4)) {
            fill_0rgb_4x(span, flat_color);
        }
        return;
    }
    // Scalar fallback — identical to the pre-batch write path.
    for i in 0..n {
        let index = indices.get(i).copied().unwrap_or(0);
        let c = if blending {
            let dst = backbuffer.get(index).copied().unwrap_or(0);
            blend_fragment(
                dst,
                rgb.get(i).copied().unwrap_or(0),
                alpha.get(i).copied().unwrap_or(0),
                frag,
            )
        } else {
            rgb.get(i).copied().unwrap_or(0)
        };
        if let Some(pixel) = backbuffer.get_mut(index) {
            *pixel = c;
        }
    }
}

/// Rasterize one triangle of already-transformed [`GuestVertex`]s.
///
/// `pre_transformed` (`XYZRHW`) vertices bypass the matrix/viewport and use
/// their X/Y as screen pixels directly. Any vertex at/behind the near plane
/// rejects the whole triangle (slice 1 limitation — no near-plane clipping).
#[allow(clippy::too_many_arguments)]
pub fn draw_triangle(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    v0: GuestVertex,
    v1: GuestVertex,
    v2: GuestVertex,
    pre_transformed: bool,
    matrix: &Mat4,
    vp: &Viewport,
    tex: Option<&TextureStage<'_>>,
    ps: Option<&PsProgram<'_>>,
    frag: &mut FragmentState<'_>,
    dirty: &mut Option<IRect>,
) {
    let to_screen = |v: GuestVertex| -> Option<ScreenVertex> {
        let (sx, sy) = if pre_transformed {
            (v.x, v.y)
        } else {
            let clip = transform_point([v.x, v.y, v.z, v.w], matrix);
            clip_to_screen(clip, vp)?
        };
        Some(ScreenVertex {
            x: sx,
            y: sy,
            z: v.z,
            color: v.color,
            u: v.u,
            v: v.v,
        })
    };
    let Some(a) = to_screen(v0) else { return };
    let Some(b) = to_screen(v1) else { return };
    let Some(c) = to_screen(v2) else { return };
    rasterize_triangle(backbuffer, width, height, a, b, c, tex, ps, frag, dirty);
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::float_cmp,
    clippy::cast_precision_loss
)]
mod tests {
    use super::{
        D3DBLEND_INVSRCALPHA, D3DBLEND_SRCALPHA, D3DBLENDOP_ADD, D3DBLENDOP_REVSUBTRACT,
        D3DBLENDOP_SUBTRACT, D3DCMP_ALWAYS, D3DCMP_EQUAL, D3DCMP_GREATER, D3DCMP_GREATEREQUAL,
        D3DCMP_LESS, D3DCMP_LESSEQUAL, D3DCMP_NEVER, D3DCMP_NOTEQUAL, D3DFVF_DIFFUSE,
        D3DFVF_NORMAL, D3DFVF_SPECULAR, D3DFVF_XYZ, D3DFVF_XYZRHW, GuestVertex, IDENTITY,
        PsFragmentInput, PsOp, PsProgram, ScreenVertex, Viewport, blend_fragment, clip_to_screen,
        depth_test, draw_triangle, is_top_or_left_edge, mat4_mul, parse_fvf, parse_vertex,
        pixel_shader_alpha_to_u8, pixel_shader_color_to_0rgb, rasterize_triangle, run_pixel_shader,
        transform_point,
    };
    use crate::d3d9_shader::{
        D3DSPDM_NONE, D3DSPDM_SATURATE, D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG,
        D3DSPSM_COMP, D3DSPSM_NEG, D3DSPSM_NONE, D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2,
        D3DSPSM_X2NEG, Operand, PsInstruction, RegType,
    };
    use crate::gdi32::IRect;

    /// A default (blend off, depth off) fragment state for rasterizer tests.
    fn no_frag() -> super::FragmentState<'static> {
        super::FragmentState {
            depth: None,
            z_enable: 0,
            z_func: super::D3DCMP_LESSEQUAL,
            z_write: 1,
            alpha_blend: 0,
            src_blend: super::D3DBLEND_ONE,
            dest_blend: super::D3DBLEND_ZERO,
            blend_op: super::D3DBLENDOP_ADD,
        }
    }

    #[test]
    fn blend_add_src_alpha_inv_src_alpha() {
        // src blue (0,0,255) at alpha 0x80 over dst red (255,0,0):
        // out = (src*128 + dst*127) >> 8 per channel.
        let frag = super::FragmentState {
            depth: None,
            z_enable: 0,
            z_func: super::D3DCMP_LESSEQUAL,
            z_write: 1,
            alpha_blend: 1,
            src_blend: D3DBLEND_SRCALPHA,
            dest_blend: D3DBLEND_INVSRCALPHA,
            blend_op: D3DBLENDOP_ADD,
        };
        let out = blend_fragment(0x00FF_0000, 0x0000_00FF, 0x80, &frag);
        let er = (255_u32 * 127) >> 8;
        let eg = 0_u32;
        let eb = (255_u32 * 128) >> 8;
        assert_eq!(
            out,
            (er << 16) | (eg << 8) | eb,
            "SRCALPHA/INVSRCALPHA ADD blend"
        );
    }

    #[test]
    fn blend_op_subtract_and_revsubtract_clamp_at_zero() {
        // Factors are 8-bit fixed point (value>>8), so ONE = 255 and
        // out = (src*255 - dst*255) >> 8.
        let sub = super::FragmentState {
            depth: None,
            z_enable: 0,
            z_func: super::D3DCMP_LESSEQUAL,
            z_write: 1,
            alpha_blend: 1,
            src_blend: super::D3DBLEND_ONE,
            dest_blend: super::D3DBLEND_ONE,
            blend_op: D3DBLENDOP_SUBTRACT,
        };
        // src red 0x80 over dst red 0x40 → (128*255 - 64*255) >> 8 = 63.
        assert_eq!(
            blend_fragment(0x0040_0000, 0x0080_0000, 0xFF, &sub),
            0x003F_0000,
            "SUBTRACT with ONE factors"
        );
        // Clamp at zero: src 0x40 over dst 0xFF → negative → 0.
        assert_eq!(
            blend_fragment(0x00FF_0000, 0x0040_0000, 0xFF, &sub),
            0x0000_0000
        );
        // REVSUBTRACT swaps the operands.
        let rev = super::FragmentState {
            depth: None,
            z_enable: 0,
            z_func: super::D3DCMP_LESSEQUAL,
            z_write: 1,
            alpha_blend: 1,
            src_blend: super::D3DBLEND_ONE,
            dest_blend: super::D3DBLEND_ONE,
            blend_op: D3DBLENDOP_REVSUBTRACT,
        };
        // src red 0x40 over dst red 0x80 → (128*255 - 64*255) >> 8 = 63.
        assert_eq!(
            blend_fragment(0x0080_0000, 0x0040_0000, 0xFF, &rev),
            0x003F_0000,
            "REVSUBTRACT swaps the operands"
        );
    }

    #[test]
    fn blend_unknown_factor_falls_back_to_one() {
        let frag = super::FragmentState {
            depth: None,
            z_enable: 0,
            z_func: super::D3DCMP_LESSEQUAL,
            z_write: 1,
            alpha_blend: 1,
            src_blend: 0xDEAD,
            dest_blend: super::D3DBLEND_ZERO,
            blend_op: D3DBLENDOP_ADD,
        };
        // Unknown src factor → ONE (255): out = (src*255 + dst*0) >> 8, i.e.
        // each channel scales by 255/256 (the documented fixed-point approx).
        assert_eq!(
            blend_fragment(0x00FF_0000, 0x0012_3456, 0xFF, &frag),
            0x0011_3355,
            "unknown factor must fall back to ONE (>>8 fixed point)"
        );
    }

    #[test]
    fn depth_test_matrix() {
        // z < existing with each compare function.
        assert!(depth_test(0.1, 0.5, D3DCMP_LESS));
        assert!(!depth_test(0.9, 0.5, D3DCMP_LESS));
        assert!(depth_test(0.5, 0.5, D3DCMP_EQUAL));
        assert!(!depth_test(0.1, 0.5, D3DCMP_EQUAL));
        assert!(depth_test(0.5, 0.5, D3DCMP_LESSEQUAL));
        assert!(depth_test(0.1, 0.5, D3DCMP_LESSEQUAL));
        assert!(!depth_test(0.9, 0.5, D3DCMP_LESSEQUAL));
        assert!(depth_test(0.9, 0.5, D3DCMP_GREATER));
        assert!(!depth_test(0.1, 0.5, D3DCMP_GREATER));
        assert!(depth_test(0.1, 0.5, D3DCMP_NOTEQUAL));
        assert!(!depth_test(0.5, 0.5, D3DCMP_NOTEQUAL));
        assert!(depth_test(0.5, 0.5, D3DCMP_GREATEREQUAL));
        assert!(depth_test(0.9, 0.5, D3DCMP_GREATEREQUAL));
        assert!(depth_test(0.9, 0.5, D3DCMP_ALWAYS));
        assert!(!depth_test(0.1, 0.5, D3DCMP_NEVER));
        // Unknown funcs pass (like ALWAYS).
        assert!(depth_test(0.1, 0.5, 0xDEAD));
    }

    #[test]
    fn fvf_xyz_diffuse_stride() {
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE).expect("valid FVF");
        assert!(!fvf.pre_transformed);
        assert!(fvf.has_diffuse);
        assert_eq!(fvf.stride, 16);
        assert_eq!(fvf.tex_coords, 0);
    }

    #[test]
    fn fvf_xyzrhw_with_normal_specular_and_tex() {
        let fvf =
            parse_fvf(D3DFVF_XYZRHW | D3DFVF_NORMAL | D3DFVF_SPECULAR | 0x0300).expect("valid FVF");
        assert!(fvf.pre_transformed);
        assert!(fvf.has_normal);
        assert!(fvf.has_specular);
        assert_eq!(fvf.tex_coords, 2);
        // 16 pos + 12 normal + 4 specular + 2 texsets * 8.
        assert_eq!(fvf.stride, 48);
    }

    #[test]
    fn fvf_requires_exactly_one_position_flag() {
        assert!(parse_fvf(D3DFVF_XYZ | D3DFVF_XYZRHW).is_none());
        assert!(parse_fvf(0).is_none());
        assert!(parse_fvf(D3DFVF_DIFFUSE).is_none());
    }

    #[test]
    fn fvf_tex_sets_must_be_contiguous() {
        // TEX1 | TEX3 (bit 0 and bit 2, gap at bit 1) → only TEX1 counts.
        let fvf = parse_fvf(D3DFVF_XYZ | 0x0500).expect("valid FVF");
        assert_eq!(fvf.tex_coords, 1);
    }

    #[test]
    fn parse_vertex_xyz_diffuse() {
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE).expect("valid FVF");
        let mut data: Vec<u8> = Vec::new();
        for value in [1.0_f32, 2.0, 3.0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&0xAA_11_22_33_u32.to_le_bytes());
        let v = parse_vertex(&data, 0, &fvf).expect("vertex in range");
        assert_eq!(v.x, 1.0);
        assert_eq!(v.y, 2.0);
        assert_eq!(v.z, 3.0);
        assert_eq!(v.w, 1.0);
        assert_eq!(v.color, 0xAA_11_22_33);
    }

    #[test]
    fn parse_vertex_skips_normal_and_uses_stride() {
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_NORMAL | D3DFVF_DIFFUSE).expect("valid FVF");
        let mut data: Vec<u8> = Vec::new();
        for vertex in 0..2 {
            for value in [1.0_f32 + vertex as f32, 2.0, 3.0] {
                data.extend_from_slice(&value.to_le_bytes());
            }
            for value in [9.0_f32, 9.0, 9.0] {
                data.extend_from_slice(&value.to_le_bytes());
            }
            data.extend_from_slice(&0xFF_01_02_03_u32.to_le_bytes());
        }
        let v = parse_vertex(&data, 0, &fvf).expect("vertex in range");
        assert_eq!(v.x, 1.0);
        assert_eq!(v.color, 0xFF_01_02_03);
        // Second vertex starts at the FVF stride.
        let v2 = parse_vertex(&data, fvf.stride as usize, &fvf).expect("vertex in range");
        assert_eq!(v2.x, 2.0);
        assert_eq!(v2.color, 0xFF_01_02_03);
    }

    #[test]
    fn parse_vertex_out_of_range_is_none() {
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE).expect("valid FVF");
        assert!(parse_vertex(&[], 0, &fvf).is_none());
    }

    #[test]
    fn fvf_xyz_diffuse_tex1_stride() {
        // XYZ(12) + DIFFUSE(4) + TEX1(8) = 24 bytes — the textured-quad FVF.
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE | 0x0100).expect("valid FVF");
        assert_eq!(fvf.tex_coords, 1);
        assert_eq!(fvf.stride, 24);
    }

    #[test]
    fn parse_vertex_reads_tex_coords() {
        let fvf = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE | 0x0100).expect("valid FVF");
        let mut data: Vec<u8> = Vec::new();
        for value in [1.0_f32, 2.0, 3.0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&0xFF_11_22_33_u32.to_le_bytes());
        for value in [0.25_f32, 0.75] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let v = parse_vertex(&data, 0, &fvf).expect("vertex in range");
        assert_eq!(v.color, 0xFF_11_22_33);
        assert_eq!(v.u, 0.25);
        assert_eq!(v.v, 0.75);
        // No TEX1 → zeroed uv.
        let fvf2 = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE).expect("valid FVF");
        let mut data2: Vec<u8> = Vec::new();
        for value in [1.0_f32, 2.0, 3.0] {
            data2.extend_from_slice(&value.to_le_bytes());
        }
        data2.extend_from_slice(&0xFF_11_22_33_u32.to_le_bytes());
        let v2 = parse_vertex(&data2, 0, &fvf2).expect("vertex in range");
        assert_eq!(v2.u, 0.0);
        assert_eq!(v2.v, 0.0);
    }

    #[test]
    fn texture_address_modes_wrap_and_clamp() {
        let stage = |pixels: &'static [u32], addr_u: u32, addr_v: u32| super::TextureStage {
            pixels,
            width: 4,
            height: 4,
            addr_u,
            addr_v,
            mag_filter: super::D3DTEXF_POINT,
            color_op: super::D3DTOP_MODULATE,
            color_arg1: super::D3DTA_TEXTURE,
            color_arg2: super::D3DTA_DIFFUSE,
            alpha_op: super::D3DTOP_MODULATE,
            alpha_arg1: super::D3DTA_TEXTURE,
            alpha_arg2: super::D3DTA_DIFFUSE,
        };
        // A 4x4 texture with texel = (y << 8) | x so the fetched index is visible.
        let mut texels = Vec::new();
        for y in 0..4_u32 {
            for x in 0..4_u32 {
                texels.push((y << 8) | x);
            }
        }
        let texels = texels.leak();
        // Wrap: u just under 1.0 maps to texel 3; negative u wraps.
        let wrap = stage(texels, super::D3DTADDRESS_WRAP, super::D3DTADDRESS_WRAP);
        assert_eq!(super::sample_texture(&wrap, 0.9, 0.1), 3);
        assert_eq!(super::sample_texture(&wrap, -0.1, 0.0), 3);
        // Clamp: out-of-range u clamps to the edge texels.
        let clamp = stage(texels, super::D3DTADDRESS_CLAMP, super::D3DTADDRESS_CLAMP);
        assert_eq!(super::sample_texture(&clamp, 2.0, 0.5), (2 << 8) | 3);
        assert_eq!(super::sample_texture(&clamp, -0.5, 2.0), 3 << 8);
    }

    #[test]
    fn color_op_evaluation() {
        let texel = 0xFF_80_40_20_u32; // r=0x80 g=0x40 b=0x20
        let diffuse = 0xFF_10_20_40_u32; // r=0x10 g=0x20 b=0x40
        // SELECTARG1 → the texel RGB.
        assert_eq!(
            super::eval_color_op(super::D3DTOP_SELECTARG1, texel, diffuse),
            0x00_80_40_20
        );
        // SELECTARG2 → the diffuse RGB.
        assert_eq!(
            super::eval_color_op(super::D3DTOP_SELECTARG2, texel, diffuse),
            0x00_10_20_40
        );
        // MODULATE: per-channel product >> 8.
        assert_eq!(
            super::eval_color_op(super::D3DTOP_MODULATE, texel, diffuse),
            ((0x80_u32 * 0x10) >> 8) << 16
                | ((0x40_u32 * 0x20) >> 8) << 8
                | ((0x20_u32 * 0x40) >> 8)
        );
    }

    #[test]
    fn identity_matrix_is_neutral() {
        let m = mat4_mul(&IDENTITY, &IDENTITY);
        assert_eq!(m, IDENTITY);
        let v = transform_point([1.0, 2.0, 3.0, 1.0], &IDENTITY);
        assert_eq!(v, [1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn transform_scale_matrix() {
        let mut m = IDENTITY;
        // Column-major: m[0] = scale x, m[5] = scale y, m[10] = scale z.
        m[0] = 2.0;
        m[5] = 3.0;
        m[10] = 0.5;
        let v = transform_point([1.0, 2.0, 4.0, 1.0], &m);
        assert_eq!(v, [2.0, 6.0, 2.0, 1.0]);
    }

    #[test]
    fn matrix_mul_is_row_vector_associative() {
        // Translation then scale, vs the pre-composed matrix.
        let mut translate = IDENTITY;
        translate[12] = 10.0; // column-major: row 0, col 3
        translate[13] = 20.0;
        let mut scale = IDENTITY;
        scale[0] = 2.0;
        scale[5] = 2.0;
        let combined = mat4_mul(&translate, &scale);
        let v = transform_point([1.0, 1.0, 0.0, 1.0], &combined);
        // (1,1) translated to (11,21), then scaled to (22,42).
        assert_eq!(v, [22.0, 42.0, 0.0, 1.0]);
    }

    #[test]
    fn clip_to_screen_maps_ndc() {
        let vp = Viewport {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
            min_z: 0.0,
            max_z: 1.0,
        };
        // NDC (-1, 1) → top-left corner.
        let (sx, sy) = clip_to_screen([-1.0, 1.0, 0.0, 1.0], &vp).expect("in front");
        assert_eq!((sx, sy), (10.0, 20.0));
        // NDC (1, -1) → bottom-right corner.
        let (sx, sy) = clip_to_screen([1.0, -1.0, 0.0, 1.0], &vp).expect("in front");
        assert_eq!((sx, sy), (110.0, 70.0));
        // w=0 (on the near plane) → rejected.
        assert!(clip_to_screen([0.0, 0.0, 0.0, 0.0], &vp).is_none());
        // w<0 (behind the camera) → rejected.
        assert!(clip_to_screen([0.0, 0.0, 0.0, -1.0], &vp).is_none());
    }

    #[test]
    fn top_left_rule_classification() {
        // Pointing right / up → top or left (boundary counts as inside).
        assert!(is_top_or_left_edge(4.0, 0.0));
        assert!(is_top_or_left_edge(0.0, -4.0));
        assert!(is_top_or_left_edge(4.0, -4.0));
        // Pointing left / down → right or bottom (boundary excluded).
        assert!(!is_top_or_left_edge(-4.0, 0.0));
        assert!(!is_top_or_left_edge(0.0, 4.0));
        assert!(!is_top_or_left_edge(-4.0, 4.0));
    }

    #[test]
    fn rasterize_fills_triangle_pixels() {
        let mut back = [0xFF_00_00_00_u32; 4 * 4];
        let mut dirty = Some(IRect::empty());
        let color = 0xFF_FF_00_00; // pure red
        rasterize_triangle(
            &mut back,
            4,
            4,
            ScreenVertex {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                color,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 4.0,
                y: 0.0,
                z: 0.0,
                color,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 0.0,
                y: 4.0,
                z: 0.0,
                color,
                u: 0.0,
                v: 0.0,
            },
            None,
            None,
            &mut no_frag(),
            &mut dirty,
        );
        // Inside pixels are red.
        assert_eq!(back[0], 0x00_FF_00_00);
        assert_eq!(back[4 + 1], 0x00_FF_00_00); // (1,1)
        // Outside (bottom-right of the diagonal) is untouched.
        assert_eq!(back[3], 0xFF_00_00_00);
        assert_eq!(back[4 + 3], 0xFF_00_00_00);
        // Dirty rect covers the bounding box.
        assert_eq!(
            dirty,
            Some(IRect {
                left: 0,
                top: 0,
                right: 4,
                bottom: 4
            })
        );
    }

    #[test]
    fn rasterize_shared_edge_single_ownership() {
        // Two triangles sharing the vertical edge x=2. With the top-left rule
        // the left triangle excludes its right edge (points down) and the
        // right triangle includes its left edge (points up) — the shared edge
        // column is drawn exactly once, by the right triangle.
        let mut back = [0xFF_00_00_00_u32; 4 * 4];
        let mut dirty = Some(IRect::empty());
        let red = 0xFF_FF_00_00;
        let blue = 0xFF_00_00_FF;
        rasterize_triangle(
            &mut back,
            4,
            4,
            ScreenVertex {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                color: red,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 2.0,
                y: 0.0,
                z: 0.0,
                color: red,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 2.0,
                y: 4.0,
                z: 0.0,
                color: red,
                u: 0.0,
                v: 0.0,
            },
            None,
            None,
            &mut no_frag(),
            &mut dirty,
        );
        rasterize_triangle(
            &mut back,
            4,
            4,
            ScreenVertex {
                x: 2.0,
                y: 0.0,
                z: 0.0,
                color: blue,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 4.0,
                y: 0.0,
                z: 0.0,
                color: blue,
                u: 0.0,
                v: 0.0,
            },
            ScreenVertex {
                x: 2.0,
                y: 4.0,
                z: 0.0,
                color: blue,
                u: 0.0,
                v: 0.0,
            },
            None,
            None,
            &mut no_frag(),
            &mut dirty,
        );
        // Row 1: left triangle interior (x in [0.75, 2]) → red; the shared
        // edge column (center x=2.5) belongs to the right triangle → blue.
        assert_eq!(back[4 + 1], 0x00_FF_00_00);
        assert_eq!(back[4 + 2], 0x00_00_00_FF);
        // Row 2: the shared edge column is still blue (right owns it).
        assert_eq!(back[8 + 2], 0x00_00_00_FF);
    }

    #[test]
    fn draw_triangle_near_plane_rejects_whole() {
        let mut back = [0xFF_00_00_00_u32; 4 * 4];
        let mut dirty = Some(IRect::empty());
        let behind = GuestVertex {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: -1.0,
            color: 0xFF_FF_00_00,
            u: 0.0,
            v: 0.0,
        };
        let ok = GuestVertex {
            x: 1.0,
            y: 1.0,
            z: 0.0,
            w: 1.0,
            color: 0xFF_FF_00_00,
            u: 0.0,
            v: 0.0,
        };
        draw_triangle(
            &mut back,
            4,
            4,
            behind,
            ok,
            ok,
            false,
            &IDENTITY,
            &Viewport {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
                min_z: 0.0,
                max_z: 1.0,
            },
            None,
            None,
            &mut no_frag(),
            &mut dirty,
        );
        assert_eq!(back, [0xFF_00_00_00; 16]);
        assert_eq!(dirty, Some(IRect::empty()));
    }

    #[test]
    fn draw_triangle_transforms_xyz_vertices() {
        // Orthographic projection mapping [-2,2]x[-2,2] to the 4x4 viewport.
        let mut proj = IDENTITY;
        proj[0] = 1.0; // x / 2
        proj[5] = 1.0; // y / 2
        let mut back = [0xFF_00_00_00_u32; 4 * 4];
        let mut dirty = Some(IRect::empty());
        let v = |x: f32, y: f32| GuestVertex {
            x,
            y,
            z: 0.0,
            w: 1.0,
            color: 0xFF_FF_FF_FF,
            u: 0.0,
            v: 0.0,
        };
        draw_triangle(
            &mut back,
            4,
            4,
            v(-2.0, -2.0),
            v(2.0, -2.0),
            v(-2.0, 2.0),
            false,
            &proj,
            &Viewport {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
                min_z: 0.0,
                max_z: 1.0,
            },
            None,
            None,
            &mut no_frag(),
            &mut dirty,
        );
        // The mapped triangle covers the whole 4x4 buffer — corner pixels fill.
        assert_eq!(back[0], 0x00_FF_FF_FF);
        assert_eq!(back[15], 0x00_FF_FF_FF);
    }

    #[test]
    fn typed_render_state_decode_round_trip() {
        // Every modeled D3DZB_* / D3DCMP_* / D3DBLEND_* / D3DBLENDOP_* value
        // must decode to its typed variant and re-encode identically.
        assert_eq!(
            super::D3dZBufferType::from_u32(super::D3DZB_FALSE),
            super::D3dZBufferType::False
        );
        assert_eq!(
            super::D3dZBufferType::from_u32(super::D3DZB_TRUE),
            super::D3dZBufferType::True
        );
        assert_eq!(
            super::D3dZBufferType::from_u32(super::D3DZB_USEW),
            super::D3dZBufferType::UseW
        );
        for func in [
            super::D3dCmpFunc::Never,
            super::D3dCmpFunc::Less,
            super::D3dCmpFunc::Equal,
            super::D3dCmpFunc::LessEqual,
            super::D3dCmpFunc::Greater,
            super::D3dCmpFunc::NotEqual,
            super::D3dCmpFunc::GreaterEqual,
            super::D3dCmpFunc::Always,
        ] {
            assert_eq!(super::D3dCmpFunc::from_u32(func.as_u32()), func);
        }
        for blend in [
            super::D3dBlend::Zero,
            super::D3dBlend::One,
            super::D3dBlend::SrcColor,
            super::D3dBlend::InvSrcColor,
            super::D3dBlend::SrcAlpha,
            super::D3dBlend::InvSrcAlpha,
            super::D3dBlend::DestAlpha,
            super::D3dBlend::InvDestAlpha,
            super::D3dBlend::DestColor,
            super::D3dBlend::InvDestColor,
        ] {
            assert_eq!(super::D3dBlend::from_u32(blend.as_u32()), blend);
        }
        for op in [
            super::D3dBlendOp::Add,
            super::D3dBlendOp::Subtract,
            super::D3dBlendOp::RevSubtract,
        ] {
            assert_eq!(super::D3dBlendOp::from_u32(op.as_u32()), op);
        }
        // Unknown guest values fall back and preserve their raw bits.
        assert_eq!(super::D3dBlend::from_u32(0xDEAD).as_u32(), 0xDEAD);
        assert_eq!(super::D3dCmpFunc::from_u32(0x1234).as_u32(), 0x1234);
    }

    #[test]
    fn render_state_defaults_match_fragment_defaults() {
        // The struct's D3D9 defaults must equal the old per-state fallbacks.
        let rs = super::RenderState::default();
        assert!(!rs.alpha_blend_enable, "ALPHABLENDENABLE default off");
        assert!(rs.z_write_enable, "ZWRITEENABLE default on");
        assert_eq!(rs.z_enable.as_u32(), super::D3DZB_FALSE);
        assert_eq!(rs.z_func.as_u32(), super::D3DCMP_LESSEQUAL);
        assert_eq!(rs.src_blend.as_u32(), super::D3DBLEND_ONE);
        assert_eq!(rs.dest_blend.as_u32(), super::D3DBLEND_ZERO);
        assert_eq!(rs.blend_op.as_u32(), super::D3DBLENDOP_ADD);
    }

    #[test]
    fn texture_stage_state_defaults() {
        let stage = super::TextureStageState::default();
        assert_eq!(stage.color_op, super::D3DTOP_MODULATE);
        assert_eq!(stage.color_arg1, super::D3DTA_TEXTURE);
        assert_eq!(stage.color_arg2, super::D3DTA_DIFFUSE);
        assert_eq!(stage.alpha_op, super::D3DTOP_MODULATE);
        assert_eq!(stage.alpha_arg1, super::D3DTA_TEXTURE);
        assert_eq!(stage.alpha_arg2, super::D3DTA_DIFFUSE);
        assert_eq!(stage.address_u, super::D3DTADDRESS_WRAP);
        assert_eq!(stage.address_v, super::D3DTADDRESS_WRAP);
        assert_eq!(stage.mag_filter, super::D3DTEXF_POINT);
        assert_eq!(stage.min_filter, super::D3DTEXF_POINT);
        assert_eq!(stage.mip_filter, super::D3DTEXF_POINT);
        assert!(stage.other_tss.is_empty());
        assert!(stage.other_sampler.is_empty());
    }

    // ── P5a: pixel-shader interpreter tests ────────────────────────────

    /// Build a one-constant program with the given instructions.
    fn program(instructions: Vec<PsInstruction>, constants: &[[f32; 4]; 32]) -> PsProgram<'static> {
        PsProgram {
            instructions: Box::leak(instructions.into_boxed_slice()),
            constants: *constants,
            samplers: [None; 4],
        }
    }

    fn end_instruction() -> PsInstruction {
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
        }
    }

    fn src(reg_type: RegType, reg_num: u16) -> Operand {
        Operand {
            reg_type,
            reg_num,
            swizzle: [0, 1, 2, 3],
            src_mod: 0,
            dst_mod: D3DSPDM_NONE,
            write_mask: 0xF,
        }
    }

    fn dst(reg_type: RegType, reg_num: u16) -> Operand {
        src(reg_type, reg_num)
    }

    fn mov(dst_reg: Operand, src_reg: Operand) -> PsInstruction {
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(dst_reg),
            srcs: vec![src_reg],
            tex_type: None,
            end: false,
        }
    }

    fn input() -> PsFragmentInput {
        PsFragmentInput {
            v0: [0.0; 4],
            v1: [0.0; 4],
            t0: [0.0, 0.0, 0.0, 1.0],
        }
    }

    #[test]
    fn interpreter_mov_const_with_saturate() {
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [0.5, 0.25, 0.125, 1.0];
        let mut sat = dst(RegType::ColorOut, 0);
        sat.dst_mod = D3DSPDM_SATURATE;
        let instrs = vec![mov(sat, src(RegType::Const, 0)), end_instruction()];
        let prog = program(instrs, &constants);
        let out = run_pixel_shader(&prog, &input()).expect("no texkill");
        assert_eq!(out, [0.5, 0.25, 0.125, 1.0]);
        assert_eq!(pixel_shader_color_to_0rgb(out), 0x00_80_40_20);
        assert_eq!(pixel_shader_alpha_to_u8(out), 255);
    }

    #[test]
    fn interpreter_write_mask_preserves_unchanged_components() {
        // mov r0.w, c0.x — only the w channel of (zeroed) r0 is written. The
        // single-component source swizzle replicates c0.x to all four source
        // channels, so the .w write lands c0.x = 9.
        let mut c0 = [0.0; 4];
        c0[0] = 9.0;
        let mut constants = [[0.0; 4]; 32];
        constants[0] = c0;
        let mut m = mov(dst(RegType::Temp, 0), src(RegType::Const, 0));
        // The `.x` source swizzle replicates c0.x to all source channels, so
        // the `.w`-masked write lands c0.x = 9.
        if let Some(s) = m.srcs.first_mut() {
            s.swizzle = [0, 0, 0, 0];
        }
        m.dst = Some(Operand {
            reg_type: RegType::Temp,
            reg_num: 0,
            swizzle: [0, 1, 2, 3],
            src_mod: 0,
            dst_mod: D3DSPDM_NONE,
            write_mask: 0x8,
        });
        let instrs = vec![
            m,
            mov(dst(RegType::ColorOut, 0), src(RegType::Temp, 0)),
            end_instruction(),
        ];
        let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
        assert_eq!(out, [0.0, 0.0, 0.0, 9.0]);
    }

    #[test]
    fn interpreter_saturate_clamps_negative() {
        let mut m = mov(dst(RegType::Temp, 0), src(RegType::Const, 0));
        m.dst = Some(Operand {
            reg_type: RegType::Temp,
            reg_num: 0,
            swizzle: [0, 1, 2, 3],
            src_mod: 0,
            dst_mod: D3DSPDM_SATURATE,
            write_mask: 0xF,
        });
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [-1.0, 2.0, 0.5, -0.25];
        let instrs = vec![
            m,
            mov(dst(RegType::ColorOut, 0), src(RegType::Temp, 0)),
            end_instruction(),
        ];
        let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
        assert_eq!(out, [0.0, 1.0, 0.5, 0.0]);
    }

    #[test]
    fn interpreter_src_modifiers() {
        // Each modifier applied to c0 = (1, 2, -3, 4), written to oC0.
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [1.0, 2.0, -3.0, 4.0];
        let cases = [
            (D3DSPSM_NONE, [1.0, 2.0, -3.0, 4.0]),
            (D3DSPSM_NEG, [-1.0, -2.0, 3.0, -4.0]),
            (D3DSPSM_BIAS, [0.5, 1.5, -3.5, 3.5]),
            (D3DSPSM_BIASNEG, [-0.5, -1.5, 3.5, -3.5]),
            (D3DSPSM_SIGN, [1.0, 1.0, -1.0, 1.0]),
            (D3DSPSM_SIGNNEG, [-1.0, -1.0, 1.0, -1.0]),
            (D3DSPSM_COMP, [0.0, -1.0, 4.0, -3.0]),
            (D3DSPSM_X2, [2.0, 4.0, -6.0, 8.0]),
            (D3DSPSM_X2NEG, [-2.0, -4.0, 6.0, -8.0]),
            (D3DSPSM_ABS, [1.0, 2.0, 3.0, 4.0]),
            (D3DSPSM_ABSNEG, [-1.0, -2.0, -3.0, -4.0]),
        ];
        for (src_mod, expected) in cases {
            let mut s = src(RegType::Const, 0);
            s.src_mod = src_mod;
            let instrs = vec![mov(dst(RegType::ColorOut, 0), s), end_instruction()];
            let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
            assert_eq!(out, expected, "src mod {src_mod}");
        }
    }

    #[test]
    fn interpreter_swizzle_components() {
        // c0 = (10, 20, 30, 40); mov oC0, c0.wzyx
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [10.0, 20.0, 30.0, 40.0];
        let mut s = src(RegType::Const, 0);
        s.swizzle = [3, 2, 1, 0];
        let instrs = vec![mov(dst(RegType::ColorOut, 0), s), end_instruction()];
        let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
        assert_eq!(out, [40.0, 30.0, 20.0, 10.0]);
    }

    #[test]
    fn interpreter_arithmetic_ops_match_reference() {
        // add/sub/mul/mad/dp3/dp4/min/max/slt/sge/frc/lrp/cmp on fixed inputs.
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [2.0, 3.0, 4.0, 5.0]; // a
        constants[1] = [3.0, 1.0, -2.0, 0.5]; // b
        constants[2] = [1.0, 1.0, 1.0, 1.0]; // c
        let run = |op: PsOp| {
            let instrs = vec![
                PsInstruction {
                    op,
                    dst: Some(dst(RegType::ColorOut, 0)),
                    srcs: vec![
                        src(RegType::Const, 0),
                        src(RegType::Const, 1),
                        src(RegType::Const, 2),
                    ],
                    tex_type: None,
                    end: false,
                },
                end_instruction(),
            ];
            run_pixel_shader(&program(instrs, &constants), &input()).expect("runs")
        };
        assert_eq!(run(PsOp::Add), [5.0, 4.0, 2.0, 5.5]);
        assert_eq!(run(PsOp::Sub), [-1.0, 2.0, 6.0, 4.5]);
        assert_eq!(run(PsOp::Mul), [6.0, 3.0, -8.0, 2.5]);
        // mad = a*b + c
        assert_eq!(run(PsOp::Mad), [7.0, 4.0, -7.0, 3.5]);
        // dp3 = a.x*b.x + a.y*b.y + a.z*b.z = 6+3-8 = 1
        assert_eq!(run(PsOp::Dp3), [1.0; 4]);
        // dp4 = 1 + 2.5 = 3.5
        assert_eq!(run(PsOp::Dp4), [3.5; 4]);
        assert_eq!(run(PsOp::Min), [2.0, 1.0, -2.0, 0.5]);
        assert_eq!(run(PsOp::Max), [3.0, 3.0, 4.0, 5.0]);
        assert_eq!(run(PsOp::Slt), [1.0, 0.0, 0.0, 0.0]); // 2<3, 3<1, 4<-2, 5<0.5
        assert_eq!(run(PsOp::Sge), [0.0, 1.0, 1.0, 1.0]);
        // lrp = a*b + (1-a)*c
        assert_eq!(
            run(PsOp::Lrp),
            [
                2.0 * 3.0 + (1.0 - 2.0) * 1.0,
                3.0 * 1.0 + (1.0 - 3.0) * 1.0,
                4.0 * -2.0 + (1.0 - 4.0) * 1.0,
                5.0 * 0.5 + (1.0 - 5.0) * 1.0
            ]
        );
        // cmp = (a >= 0) ? b : c — a is all-positive here → b
        assert_eq!(run(PsOp::Cmp), [3.0, 1.0, -2.0, 0.5]);
    }

    #[test]
    fn interpreter_cmp_selects_on_sign() {
        // c0 = (-1, 1, 0, -0.5): cmp picks c where negative, b where >= 0.
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [-1.0, 1.0, 0.0, -0.5];
        constants[1] = [10.0, 10.0, 10.0, 10.0]; // b (selected when a >= 0)
        constants[2] = [20.0, 20.0, 20.0, 20.0]; // c
        let instrs = vec![
            PsInstruction {
                op: PsOp::Cmp,
                dst: Some(dst(RegType::ColorOut, 0)),
                srcs: vec![
                    src(RegType::Const, 0),
                    src(RegType::Const, 1),
                    src(RegType::Const, 2),
                ],
                tex_type: None,
                end: false,
            },
            end_instruction(),
        ];
        let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
        assert_eq!(out, [20.0, 10.0, 10.0, 20.0]);
    }

    #[test]
    fn interpreter_scalar_ops() {
        let run = |op: PsOp, c0: [f32; 4]| {
            let mut constants = [[0.0; 4]; 32];
            constants[0] = c0;
            let instrs = vec![
                PsInstruction {
                    op,
                    dst: Some(dst(RegType::ColorOut, 0)),
                    srcs: vec![src(RegType::Const, 0)],
                    tex_type: None,
                    end: false,
                },
                end_instruction(),
            ];
            run_pixel_shader(&program(instrs, &constants), &input()).expect("runs")
        };
        assert_eq!(run(PsOp::Rcp, [4.0, 99.0, 99.0, 99.0]), [0.25; 4]);
        assert_eq!(run(PsOp::Rsq, [4.0, 99.0, 99.0, 99.0]), [0.5; 4]); // 1/sqrt(4)
        assert_eq!(
            run(PsOp::Exp, [4.0, 99.0, 99.0, 99.0]),
            [16.0, 99.0, 99.0, 99.0]
        ); // 2^4, yzw copied
        assert_eq!(
            run(PsOp::Log, [4.0, 99.0, 99.0, 99.0]),
            [2.0, 99.0, 99.0, 99.0]
        ); // log2(4), yzw copied
        // frc applies per component: (4.5, 99, 99, 99) - floor → (0.5, 0, 0, 0).
        assert_eq!(
            run(PsOp::Frc, [4.5, 99.0, 99.0, 99.0]),
            [0.5, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn interpreter_rcp_rsq_domain_edges() {
        // rcp(0) = +inf, rsq(0) = +inf, rsq(-4) = 0.5, log2(0) = -inf.
        let run = |op: PsOp, c: f32| {
            let mut constants = [[0.0; 4]; 32];
            constants[0] = [c, 0.0, 0.0, 0.0];
            let instrs = vec![
                PsInstruction {
                    op,
                    dst: Some(dst(RegType::ColorOut, 0)),
                    srcs: vec![src(RegType::Const, 0)],
                    tex_type: None,
                    end: false,
                },
                end_instruction(),
            ];
            run_pixel_shader(&program(instrs, &constants), &input()).expect("runs")[0]
        };
        assert!(run(PsOp::Rcp, 0.0).is_infinite());
        assert_eq!(run(PsOp::Rcp, 2.0), 0.5);
        assert!(run(PsOp::Rsq, 0.0).is_infinite());
        assert_eq!(run(PsOp::Rsq, -4.0), 0.5);
        assert!(run(PsOp::Log, 0.0).is_infinite());
        assert!(run(PsOp::Log, -1.0).is_nan());
        assert_eq!(run(PsOp::Exp, 0.0), 1.0);
    }

    #[test]
    fn interpreter_texkill_discards_on_negative_component() {
        // texkill t0 with t0.z = -1 → the fragment is discarded.
        let constants = [[0.0; 4]; 32];
        let instrs = vec![
            PsInstruction {
                op: PsOp::TexKill,
                dst: None,
                srcs: vec![src(RegType::Texture, 0)],
                tex_type: None,
                end: false,
            },
            end_instruction(),
        ];
        let prog = program(instrs, &constants);
        assert!(
            run_pixel_shader(
                &prog,
                &PsFragmentInput {
                    v0: [0.0; 4],
                    v1: [0.0; 4],
                    t0: [0.5, 0.5, -1.0, 1.0]
                }
            )
            .is_none()
        );
        assert!(
            run_pixel_shader(
                &prog,
                &PsFragmentInput {
                    v0: [0.0; 4],
                    v1: [0.0; 4],
                    t0: [0.5, 0.5, 1.0, 1.0]
                }
            )
            .is_some()
        );
    }

    #[test]
    fn interpreter_mov_oc0_via_constant_shader_like_micro_exe() {
        // The exact micro-exe shader: def c0,0,0,0,0 → mov oC0, c0 → end,
        // with c0 overridden by SetPixelShaderConstantF to (0.5,0.25,0.125,1).
        let mut constants = [[0.0; 4]; 32];
        constants[0] = [0.5, 0.25, 0.125, 1.0];
        let instrs = vec![
            mov(dst(RegType::ColorOut, 0), src(RegType::Const, 0)),
            end_instruction(),
        ];
        let out = run_pixel_shader(&program(instrs, &constants), &input()).expect("runs");
        assert_eq!(out, [0.5, 0.25, 0.125, 1.0]);
        assert_eq!(pixel_shader_color_to_0rgb(out), 0x00_80_40_20);
    }

    #[test]
    fn interpreter_unbound_sampler_texld_writes_zero() {
        // texld r0, t0, s0 with no texture bound → oC0 = (0,0,0,0).
        let instrs = vec![
            PsInstruction {
                op: PsOp::Tex,
                dst: Some(dst(RegType::Temp, 0)),
                srcs: vec![src(RegType::Texture, 0), src(RegType::Sampler, 0)],
                tex_type: None,
                end: false,
            },
            mov(dst(RegType::ColorOut, 0), src(RegType::Temp, 0)),
            end_instruction(),
        ];
        let prog = PsProgram {
            instructions: Box::leak(instrs.into_boxed_slice()),
            constants: [[0.0; 4]; 32],
            samplers: [None; 4],
        };
        let out = run_pixel_shader(&prog, &input()).expect("runs");
        assert_eq!(out, [0.0; 4]);
    }
}
