// ── Fixed-function lighting (GL 1.1, per-vertex Gouraud) ────────────────
//
// State: up to eight lights (ambient/diffuse/specular, eye-space position,
// attenuation), the light-model ambient, and the front/back material.
// The lit color is computed at vertex time (immediate mode `glVertex` and
// array draws both route through [`lit_vertex_color`]); GL_SMOOTH shades per
// vertex, GL_FLAT uses the first vertex of each primitive (handled by the
// emission core).
//
// Documented approximations: the normal is transformed by the inverse
// transpose of the MODELVIEW upper 3×3 (the exact GL rule; the 3×3 inverse is
// the adjugate over the determinant, so non-invertible matrices fall back to
// the plain upper 3×3), the eye-space view vector uses the ortho vertex
// position (GL's view vector is from the vertex to the eye — for the
// orthographic projections legacy apps use, this is the vertex position in
// the ortho space), and `GL_LIGHT_MODEL_TWO_SIDE` is accepted but ignored
// (back faces are lit with the same material — a documented gap).

use super::lists::{ListOp, gl_capture};
use super::*;

/// `GL_LIGHTING` (gl.h).
pub(crate) const GL_LIGHTING: u32 = 0x0B50;
/// `GL_LIGHT0` — the first light enable enum (0x4000..0x4007).
pub(crate) const GL_LIGHT0: u32 = 0x4000;
/// `GL_LIGHT_MODEL_AMBIENT`.
pub(crate) const GL_LIGHT_MODEL_AMBIENT: u32 = 0x0B53;
/// `GL_LIGHT_MODEL_TWO_SIDE`.
pub(crate) const GL_LIGHT_MODEL_TWO_SIDE: u32 = 0x0B52;
/// `GL_AMBIENT`.
pub(crate) const GL_AMBIENT: u32 = 0x1200;
/// `GL_DIFFUSE`.
pub(crate) const GL_DIFFUSE: u32 = 0x1201;
/// `GL_SPECULAR`.
pub(crate) const GL_SPECULAR: u32 = 0x1202;
/// `GL_POSITION`.
pub(crate) const GL_POSITION: u32 = 0x1203;
/// `GL_CONSTANT_ATTENUATION`.
pub(crate) const GL_CONSTANT_ATTENUATION: u32 = 0x1207;
/// `GL_LINEAR_ATTENUATION`.
pub(crate) const GL_LINEAR_ATTENUATION: u32 = 0x1208;
/// `GL_QUADRATIC_ATTENUATION`.
pub(crate) const GL_QUADRATIC_ATTENUATION: u32 = 0x1209;
/// `GL_EMISSION`.
pub(crate) const GL_EMISSION: u32 = 0x1600;
/// `GL_SHININESS`.
pub(crate) const GL_SHININESS: u32 = 0x1601;
/// `GL_FRONT` (material face).
pub(crate) const GL_FRONT: u32 = 0x0404;
/// `GL_SMOOTH` shade model.
pub(crate) const GL_SMOOTH: u32 = 0x1D01;
/// `GL_FLAT` shade model.
pub(crate) const GL_FLAT: u32 = 0x1D00;

/// One light source.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LightState {
    /// `glEnable(GL_LIGHTi)`.
    pub(crate) enabled: bool,
    /// `GL_AMBIENT` (default `(0, 0, 0, 1)`).
    pub(crate) ambient: [f32; 4],
    /// `GL_DIFFUSE` (default `(1, 1, 1, 1)` for LIGHT0, black otherwise).
    pub(crate) diffuse: [f32; 4],
    /// `GL_SPECULAR` (default `(1, 1, 1, 1)` for LIGHT0, black otherwise).
    pub(crate) specular: [f32; 4],
    /// `GL_POSITION` in EYE space (transformed by the modelview at
    /// `glLightfv` time; w = 0 → directional, w = 1 → positional).
    pub(crate) position: [f32; 4],
    /// `GL_CONSTANT_ATTENUATION` (default 1).
    pub(crate) constant_attenuation: f32,
    /// `GL_LINEAR_ATTENUATION` (default 0).
    pub(crate) linear_attenuation: f32,
    /// `GL_QUADRATIC_ATTENUATION` (default 0).
    pub(crate) quadratic_attenuation: f32,
}

impl Default for LightState {
    fn default() -> Self {
        Self {
            enabled: false,
            ambient: [0.0, 0.0, 0.0, 1.0],
            diffuse: [1.0, 1.0, 1.0, 1.0],
            specular: [1.0, 1.0, 1.0, 1.0],
            position: [0.0, 0.0, 1.0, 0.0],
            constant_attenuation: 1.0,
            linear_attenuation: 0.0,
            quadratic_attenuation: 0.0,
        }
    }
}

/// The lighting state added to [`GlCtx`] (fields live in the parent struct;
/// the fns here read/write them through `super`).
impl GlCtx {
    /// `glEnable(GL_LIGHTi)` / `glDisable` — sets the light's enabled flag.
    pub(crate) fn set_light_enabled(&mut self, light: u32, enabled: bool) {
        let index = usize::try_from(light.wrapping_sub(GL_LIGHT0)).unwrap_or(usize::MAX);
        if let Some(slot) = self.lights.get_mut(index) {
            slot.enabled = enabled;
        }
    }

    /// `glLightfv(light, pname, params)` — params is 1 or 4 floats.
    pub(crate) fn light_fv(&mut self, light: u32, pname: u32, params: &[f32]) {
        let p = four(params);
        if !gl_capture(
            self,
            ListOp::LightFv {
                light,
                pname,
                params: p,
            },
        ) {
            return;
        }
        let params = p.as_slice();
        let index = usize::try_from(light.wrapping_sub(GL_LIGHT0)).unwrap_or(usize::MAX);
        let Some(slot) = self.lights.get_mut(index) else {
            self.set_error(GL_INVALID_ENUM);
            return;
        };
        match pname {
            GL_AMBIENT => slot.ambient = four(params),
            GL_DIFFUSE => slot.diffuse = four(params),
            GL_SPECULAR => slot.specular = four(params),
            GL_POSITION => {
                // Transform by the CURRENT modelview: the light position is
                // fixed in eye space for the frame's lighting.
                let mv = self.modelview_stack.last().copied().unwrap_or(IDENTITY);
                let p = four(params);
                let transformed = transform_point([p[0], p[1], p[2], p[3]], &mv);
                slot.position = transformed;
            }
            GL_CONSTANT_ATTENUATION => {
                slot.constant_attenuation = params.first().copied().unwrap_or(1.0)
            }
            GL_LINEAR_ATTENUATION => {
                slot.linear_attenuation = params.first().copied().unwrap_or(0.0)
            }
            GL_QUADRATIC_ATTENUATION => {
                slot.quadratic_attenuation = params.first().copied().unwrap_or(0.0);
            }
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    /// `glLightModelfv(pname, params)` — ambient (4f) / two-side (1f).
    pub(crate) fn light_model_fv(&mut self, pname: u32, params: &[f32]) {
        let p = four(params);
        if !gl_capture(self, ListOp::LightModelFv { pname, params: p }) {
            return;
        }
        let params = p.as_slice();
        match pname {
            GL_LIGHT_MODEL_AMBIENT => self.light_model_ambient = four(params),
            GL_LIGHT_MODEL_TWO_SIDE => {
                let on = params.first().copied().unwrap_or(0.0) != 0.0;
                if on {
                    // Accepted but ignored: back faces use the same material.
                    tracing::debug!(target: "wiegui", "glLightModel: TWO_SIDE accepted-and-ignored");
                }
            }
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    /// `glMaterialfv(face, pname, params)` — 4-float material components.
    pub(crate) fn material_fv(&mut self, face: u32, pname: u32, params: &[f32]) {
        let p = four(params);
        if !gl_capture(
            self,
            ListOp::MaterialFv {
                face,
                pname,
                params: p,
            },
        ) {
            return;
        }
        if !matches!(face, GL_FRONT | GL_FRONT_AND_BACK) {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        match pname {
            GL_AMBIENT => self.material_ambient = four(params),
            GL_DIFFUSE => self.material_diffuse = four(params),
            GL_SPECULAR => self.material_specular = four(params),
            GL_EMISSION => self.material_emission = four(params),
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    /// `glMaterialf(face, pname, param)` — the scalar `GL_SHININESS`.
    pub(crate) fn material_f(&mut self, face: u32, pname: u32, param: f32) {
        if !gl_capture(self, ListOp::MaterialF { face, pname, param }) {
            return;
        }
        if !matches!(face, GL_FRONT | GL_FRONT_AND_BACK) {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        match pname {
            GL_SHININESS => self.material_shininess = param.clamp(0.0, 128.0),
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    /// The lit vertex color (emission + ambient + Σ lights), 0..1 each.
    ///
    /// `pos` is the object-space vertex (transformed to eye space by the
    /// modelview), `normal` the object-space vertex normal, `fallback` the
    /// material alpha source when the material alpha is 1 (the GL rule: the
    /// lit color's alpha is the material's alpha). When `GL_LIGHTING` is off
    /// this returns the caller's unlit color unchanged.
    #[must_use]
    pub(crate) fn lit_color(&self, pos: [f32; 4], normal: [f32; 3], base: [f32; 4]) -> [f32; 4] {
        if !self.lighting {
            return base;
        }
        let mv = self.modelview_stack.last().copied().unwrap_or(IDENTITY);
        let eye = transform_point([pos[0], pos[1], pos[2], 1.0], &mv);
        let n = normalize3x3(transform_normal(normal, &mv));
        let mut color = self.material_emission;
        let ambient_model = self.light_model_ambient;
        for (i, c) in color.iter_mut().enumerate() {
            *c += ambient_model[i] * self.material_ambient[i];
        }
        let mut light_on = false;
        for light in &self.lights {
            if !light.enabled {
                continue;
            }
            light_on = true;
            // Direction FROM the vertex TO the light + the distance.
            let (lx, ly, lz, dir_len) = if light.position[3] == 0.0 {
                // Directional: the position is the direction to the light.
                let (x, y, z) = (light.position[0], light.position[1], light.position[2]);
                let len = (x * x + y * y + z * z).sqrt();
                let len = if len < 1.0e-6 { 1.0 } else { len };
                (x / len, y / len, z / len, f32::INFINITY)
            } else {
                let vx = light.position[0] - eye[0];
                let vy = light.position[1] - eye[1];
                let vz = light.position[2] - eye[2];
                let d = (vx * vx + vy * vy + vz * vz).sqrt();
                if d < 1.0e-6 {
                    (vx, vy, vz, d.max(1.0e-6))
                } else {
                    (vx / d, vy / d, vz / d, d)
                }
            };
            let ndotl = (n[0] * lx + n[1] * ly + n[2] * lz).max(0.0);
            let atten = if dir_len.is_infinite() {
                1.0
            } else {
                let d = dir_len;
                let denom = light.constant_attenuation
                    + light.linear_attenuation * d
                    + light.quadratic_attenuation * d * d;
                if denom < 1.0e-6 { 1.0 } else { 1.0 / denom }
            };
            // Specular: H = normalize(L + V), V = eye → vertex.
            let (vx, vy, vz) = (-eye[0], -eye[1], -eye[2]);
            let vlen = (vx * vx + vy * vy + vz * vz).sqrt().max(1.0e-6);
            let (vx, vy, vz) = (vx / vlen, vy / vlen, vz / vlen);
            let (hx, hy, hz) = (lx + vx, ly + vy, lz + vz);
            let hlen = (hx * hx + hy * hy + hz * hz).sqrt().max(1.0e-6);
            let ndoth = ((n[0] * hx + n[1] * hy + n[2] * hz) / hlen).max(0.0);
            let spec_factor = if self.material_shininess > 0.0 && ndotl > 0.0 {
                ndoth.powf(self.material_shininess)
            } else {
                0.0
            };
            let mut contrib = [0.0_f32; 4];
            for (i, c) in contrib.iter_mut().enumerate() {
                *c += light.ambient[i] * self.material_ambient[i];
                *c += light.diffuse[i] * self.material_diffuse[i] * ndotl;
                *c += light.specular[i] * self.material_specular[i] * spec_factor;
            }
            for (i, c) in color.iter_mut().enumerate() {
                *c += atten * contrib[i];
            }
        }
        let _ = light_on; // reserved: emission-only fallback for no lights
        color[0] = color[0].clamp(0.0, 1.0);
        color[1] = color[1].clamp(0.0, 1.0);
        color[2] = color[2].clamp(0.0, 1.0);
        color[3] = self.material_diffuse[3];
        color
    }
}

/// First four params (missing → the GL default for the pname's alpha is 1).
#[must_use]
fn four(params: &[f32]) -> [f32; 4] {
    let get = |i: usize| {
        params
            .get(i)
            .copied()
            .unwrap_or(if i == 3 { 1.0 } else { 0.0 })
    };
    [get(0), get(1), get(2), get(3)]
}

/// Transform a normal by the inverse-transpose of the modelview upper 3×3.
#[must_use]
fn transform_normal(n: [f32; 3], mv: &Mat4) -> [f32; 3] {
    // Upper 3×3 of the column-major modelview: M[r][c] = m[c*4+r].
    let m00 = mv[0];
    let m01 = mv[4];
    let m02 = mv[8];
    let m10 = mv[1];
    let m11 = mv[5];
    let m12 = mv[9];
    let m20 = mv[2];
    let m21 = mv[6];
    let m22 = mv[10];
    // Inverse-transpose = adjugate of the transpose over the determinant of
    // the transpose (the inverse of M is adj(M)/det(M); (M⁻¹)ᵀ = adj(M)ᵀ/det).
    let a = m00;
    let b = m01;
    let c = m02;
    let d = m10;
    let e = m11;
    let f = m12;
    let g = m20;
    let h = m21;
    let i = m22;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1.0e-12 {
        // Singular (e.g. a degenerate projection folded in): fall back to
        // the plain upper 3×3 (normalized downstream).
        return [
            m00 * n[0] + m01 * n[1] + m02 * n[2],
            m10 * n[0] + m11 * n[1] + m12 * n[2],
            m20 * n[0] + m21 * n[1] + m22 * n[2],
        ];
    }
    let inv_det = 1.0 / det;
    // Cofactor matrix of M (the adjugate of Mᵀ = (adj(M))ᵀ).
    let c00 = (e * i - f * h) * inv_det;
    let c01 = (c * h - b * i) * inv_det;
    let c02 = (b * f - c * e) * inv_det;
    let c10 = (f * g - d * i) * inv_det;
    let c11 = (a * i - c * g) * inv_det;
    let c12 = (c * d - a * f) * inv_det;
    let c20 = (d * h - e * g) * inv_det;
    let c21 = (b * g - a * h) * inv_det;
    let c22 = (a * e - b * d) * inv_det;
    [
        c00 * n[0] + c01 * n[1] + c02 * n[2],
        c10 * n[0] + c11 * n[1] + c12 * n[2],
        c20 * n[0] + c21 * n[1] + c22 * n[2],
    ]
}

/// Normalize a 3-vector (zero vector stays zero).
#[must_use]
fn normalize3x3(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1.0e-6 {
        v
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}
