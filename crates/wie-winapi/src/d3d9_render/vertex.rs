//! FVF layout parsing, vertex decoding, and the fixed-function transform
//! (matrix multiply, clip→screen mapping).

use super::{D3DFVF_DIFFUSE, D3DFVF_NORMAL, D3DFVF_SPECULAR, D3DFVF_XYZ, D3DFVF_XYZRHW};

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
/// behind the near-plane (`w <= 0`) — slice 1 rejects the whole triangle.
///
/// `#[expect(casts)]`: viewport fields are `u32` but the mapping is float
/// math — `f32: From<u32>` does not exist in `std`, and a 32-bit viewport
/// coordinate (≤ ~2^24 px) cannot lose precision in `f32`'s 23-bit mantissa.
#[must_use]
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
