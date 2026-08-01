//! P3: D3D9 software-render core — FVF layout parsing, vertex transform,
//! and solid-fill triangle rasterization.
//!
//! Slice 1 scope (roadmap B6 slice 1): `D3DFVF_XYZ` / `D3DFVF_XYZRHW`
//! positions, optional `D3DFVF_NORMAL` / `D3DFVF_DIFFUSE` / `D3DFVF_SPECULAR`,
//! flat-vertex diffuse colors, a fixed-function world × view × projection
//! transform, near-plane rejection, and a top-left-rule fill with barycentric
//! color interpolation. No textures, lighting, or z-buffer (draw order).

use crate::gdi32::IRect;

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
    // Texture coordinates are skipped (unused in slice 1).
    Some(GuestVertex { x, y, z, w, color })
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
    /// Diffuse color `0xAARRGGBB`.
    pub color: u32,
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

/// Blend three `0xAARRGGBB` colors by barycentric weights; drops alpha (0RGB).
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
    let rb = f32::from(u8::try_from((cb >> 16) & 0xFF).unwrap_or(0));
    let gb = f32::from(u8::try_from((cb >> 8) & 0xFF).unwrap_or(0));
    let bb = f32::from(u8::try_from(cb & 0xFF).unwrap_or(0));
    let rc = f32::from(u8::try_from((cc >> 16) & 0xFF).unwrap_or(0));
    let gc = f32::from(u8::try_from((cc >> 8) & 0xFF).unwrap_or(0));
    let bc = f32::from(u8::try_from(cc & 0xFF).unwrap_or(0));
    let r = (wa * ra + wb * rb + wc * rc).round() as u32;
    let g = (wa * ga + wb * gb + wc * gc).round() as u32;
    let b = (wa * ba + wb * bb + wc * bc).round() as u32;
    (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

/// Fill one triangle into `backbuffer` (0RGB, top-down, `width` × `height`)
/// with barycentric solid-fill, accumulating the covered region into `dirty`.
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
    clippy::cast_possible_truncation
)]
pub fn rasterize_triangle(
    backbuffer: &mut [u32],
    width: u32,
    height: u32,
    a: ScreenVertex,
    b: ScreenVertex,
    c: ScreenVertex,
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

    let width_us = usize::try_from(width).unwrap_or(0);
    for py in y0..y1 {
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
            let color = blend_colors(wa, a.color, wb, b.color, wc, c.color);
            let index = usize::try_from(py)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(px).unwrap_or(0));
            if let Some(pixel) = backbuffer.get_mut(index) {
                *pixel = color;
            }
        }
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
            color: v.color,
        })
    };
    let Some(a) = to_screen(v0) else { return };
    let Some(b) = to_screen(v1) else { return };
    let Some(c) = to_screen(v2) else { return };
    rasterize_triangle(backbuffer, width, height, a, b, c, dirty);
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
        D3DFVF_DIFFUSE, D3DFVF_NORMAL, D3DFVF_SPECULAR, D3DFVF_XYZ, D3DFVF_XYZRHW, GuestVertex,
        IDENTITY, ScreenVertex, Viewport, clip_to_screen, draw_triangle, is_top_or_left_edge,
        mat4_mul, parse_fvf, parse_vertex, rasterize_triangle, transform_point,
    };
    use crate::gdi32::IRect;

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
                color,
            },
            ScreenVertex {
                x: 4.0,
                y: 0.0,
                color,
            },
            ScreenVertex {
                x: 0.0,
                y: 4.0,
                color,
            },
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
                color: red,
            },
            ScreenVertex {
                x: 2.0,
                y: 0.0,
                color: red,
            },
            ScreenVertex {
                x: 2.0,
                y: 4.0,
                color: red,
            },
            &mut dirty,
        );
        rasterize_triangle(
            &mut back,
            4,
            4,
            ScreenVertex {
                x: 2.0,
                y: 0.0,
                color: blue,
            },
            ScreenVertex {
                x: 4.0,
                y: 0.0,
                color: blue,
            },
            ScreenVertex {
                x: 2.0,
                y: 4.0,
                color: blue,
            },
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
        };
        let ok = GuestVertex {
            x: 1.0,
            y: 1.0,
            z: 0.0,
            w: 1.0,
            color: 0xFF_FF_00_00,
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
            &mut dirty,
        );
        // The mapped triangle covers the whole 4x4 buffer — corner pixels fill.
        assert_eq!(back[0], 0x00_FF_FF_FF);
        assert_eq!(back[15], 0x00_FF_FF_FF);
    }
}
