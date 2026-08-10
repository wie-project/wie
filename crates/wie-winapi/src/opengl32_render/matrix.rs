// ── Matrix construction (GL column-major conventions) ───────────────────
//
// The standard GL projection/transform matrices as `Mat4` arrays. GL's
// column-major storage feeds the shared `d3d9_render` helpers unchanged (see
// the module doc on the transform conventions).

use super::*;

// ── Matrix construction (GL column-major conventions) ───────────────────

/// `glOrtho(l, r, b, t, n, f)` — maps NDC z to [-1, 1] (GL convention; the
/// viewport stage re-maps to the [0, 1] depth range).
#[must_use]
pub(super) fn ortho_matrix(l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) -> Mat4 {
    let rl = r - l;
    let tb = t - b;
    let fn_ = f - n;
    if rl == 0.0 || tb == 0.0 || fn_ == 0.0 {
        return IDENTITY;
    }
    let mut m = IDENTITY;
    m[0] = 2.0 / rl;
    m[5] = 2.0 / tb;
    m[10] = -2.0 / fn_;
    m[12] = -(r + l) / rl;
    m[13] = -(t + b) / tb;
    m[14] = -(f + n) / fn_;
    m
}

/// `glFrustum(l, r, b, t, n, f)`.
#[must_use]
pub(super) fn frustum_matrix(l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) -> Mat4 {
    let rl = r - l;
    let tb = t - b;
    let fn_ = f - n;
    if rl == 0.0 || tb == 0.0 || fn_ == 0.0 || n == 0.0 {
        return IDENTITY;
    }
    let mut m = [0.0_f32; 16];
    m[0] = 2.0 * n / rl;
    m[5] = 2.0 * n / tb;
    m[8] = (r + l) / rl;
    m[9] = (t + b) / tb;
    m[10] = -(f + n) / fn_;
    m[11] = -1.0;
    m[14] = -2.0 * f * n / fn_;
    m
}

/// `glTranslatef(x, y, z)`.
#[must_use]
pub(super) fn translate_matrix(x: f32, y: f32, z: f32) -> Mat4 {
    let mut m = IDENTITY;
    m[12] = x;
    m[13] = y;
    m[14] = z;
    m
}

/// `glScalef(x, y, z)`.
#[must_use]
pub(super) fn scale_matrix(x: f32, y: f32, z: f32) -> Mat4 {
    let mut m = IDENTITY;
    m[0] = x;
    m[5] = y;
    m[10] = z;
    m
}

/// `glRotatef(angle, x, y, z)` — axis-angle rotation, `angle` in degrees.
#[must_use]
pub(super) fn rotate_matrix(angle: f32, x: f32, y: f32, z: f32) -> Mat4 {
    let len_sq = x * x + y * y + z * z;
    if len_sq < 1.0e-8 {
        // Zero axis: GL leaves the matrix undefined; identity is the safe
        // choice.
        return IDENTITY;
    }
    let inv = 1.0 / len_sq.sqrt();
    let (x, y, z) = (x * inv, y * inv, z * inv);
    let rad = angle.to_radians();
    let c = rad.cos();
    let s = rad.sin();
    let one_minus = 1.0 - c;
    let mut m = [0.0_f32; 16];
    m[0] = c + x * x * one_minus;
    m[1] = x * y * one_minus + z * s;
    m[2] = x * z * one_minus - y * s;
    m[4] = x * y * one_minus - z * s;
    m[5] = c + y * y * one_minus;
    m[6] = y * z * one_minus + x * s;
    m[8] = x * z * one_minus + y * s;
    m[9] = y * z * one_minus - x * s;
    m[10] = c + z * z * one_minus;
    m[15] = 1.0;
    m
}
