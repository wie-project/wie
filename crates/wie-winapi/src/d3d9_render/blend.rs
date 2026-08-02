//! Typed blend + depth render-state enums, the depth test, and the
//! `D3DBLEND_*` fixed-point blend.

use super::sample::FragmentState;
use super::{
    D3DBLEND_DESTALPHA, D3DBLEND_DESTCOLOR, D3DBLEND_INVDESTCOLOR, D3DBLEND_INVSRCALPHA,
    D3DBLEND_INVSRCCOLOR, D3DBLEND_SRCALPHA, D3DBLEND_SRCCOLOR, D3DBLEND_ZERO,
    D3DBLENDOP_REVSUBTRACT, D3DBLENDOP_SUBTRACT, D3DCMP_EQUAL, D3DCMP_GREATER, D3DCMP_GREATEREQUAL,
    D3DCMP_LESS, D3DCMP_LESSEQUAL, D3DCMP_NEVER, D3DCMP_NOTEQUAL,
};

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
/// Depth compare against `D3DRS_ZFUNC`.
///
/// `#[expect(float_cmp)]`: the exact `==`/`!=` compares are the D3D9 EQUAL /
/// NOTEQUAL semantics on the unquantized software depth values.
#[must_use]
pub(super) fn depth_test(z: f32, existing: f32, func: u32) -> bool {
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
pub(super) fn blend_fragment(dst: u32, src: u32, alpha: u8, frag: &FragmentState<'_>) -> u32 {
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
pub(super) fn edge_inside(e: f32, dx: f32, dy: f32) -> bool {
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
pub(super) fn blend_colors(wa: f32, ca: u32, wb: f32, cb: u32, wc: f32, cc: u32) -> u32 {
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
