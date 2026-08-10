use super::*;

/// A 32×32 context with an identity ortho(-1,1,-1,1) projection, so NDC
/// maps directly to screen pixels: NDC (0,0) → pixel (16,16).
fn test_ctx() -> GlCtx {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 32, 32);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    gl_ortho(&mut ctx, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    ctx
}

fn pixel(ctx: &GlCtx, x: usize, y: usize) -> u32 {
    ctx.backbuffer
        .get(y.saturating_mul(32).saturating_add(x))
        .copied()
        .unwrap_or(0)
}

#[test]
fn clear_fills_color_and_depth() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.1, 0.1, 0.4, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    // 0.1*255 = 25.5 → 26 (0x1A); 0.4*255 = 102 (0x66).
    assert_eq!(pixel(&ctx, 0, 0), 0xFF_1A_1A_66);
    assert_eq!(pixel(&ctx, 31, 31), 0xFF_1A_1A_66);
    assert_eq!(*ctx.depth.first().unwrap_or(&-1.0), 1.0);
}

#[test]
fn triangle_covers_pixels_with_interpolated_color() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_color(&mut ctx, 1.0, 0.0, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0); // screen (8, 8)
    gl_color(&mut ctx, 0.0, 1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0); // screen (24, 8)
    gl_color(&mut ctx, 0.0, 0.0, 1.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0); // screen (8, 24)
    gl_end(&mut ctx);
    // Centroid pixel (13, 13), barycentric (80, 88, 88)/256:
    // r = 80/256·255 = 79.7 → 80, g = b = 88/256·255 = 87.7 → 88.
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_50_58_58,
        "interpolated centroid color"
    );
    // Near the red vertex: (207, 24, 24).
    assert_eq!(
        pixel(&ctx, 9, 9),
        0xFF_CF_18_18,
        "red-weighted interior pixel"
    );
    // Outside the triangle (bottom-right): clear color remains.
    assert_eq!(pixel(&ctx, 30, 30), 0xFF_00_00_00, "outside stays clear");
}

#[test]
fn depth_test_hides_occluded_triangle() {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 32, 32);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    // n=0, f=1: glOrtho maps eye z ∈ [-f, -n] = [-1, 0] to NDC [-1, 1]
    // (GL eye space looks down -z). Eye z -0.1 → depth 0.1 (near),
    // -0.9 → depth 0.9 (far).
    gl_ortho(&mut ctx, -1.0, 1.0, -1.0, 1.0, 0.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    gl_enable(&mut ctx, GL_DEPTH_TEST);
    let tri = |ctx: &mut GlCtx, z: f32, c: [f32; 4]| {
        gl_color(ctx, c[0], c[1], c[2], c[3]);
        gl_begin(ctx, GL_TRIANGLES);
        gl_vertex(ctx, -1.0, -1.0, z, 1.0);
        gl_vertex(ctx, 1.0, -1.0, z, 1.0);
        gl_vertex(ctx, 0.0, 1.0, z, 1.0);
        gl_end(ctx);
    };
    // Near green triangle first, then a far red one over the same region.
    tri(&mut ctx, -0.1, [0.0, 1.0, 0.0, 1.0]);
    tri(&mut ctx, -0.9, [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(
        pixel(&ctx, 16, 16),
        0xFF_00_FF_00,
        "far triangle must fail GL_LESS"
    );
    let d = ctx
        .depth
        .get(16_usize.saturating_mul(32).saturating_add(16))
        .copied()
        .unwrap_or(-1.0);
    assert!(
        (d - 0.1).abs() < 1.0e-6,
        "depth holds the near value, got {d}"
    );
}

#[test]
fn alpha_blend_quad_over_red_clear() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 1.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_enable(&mut ctx, GL_BLEND);
    // Default SRC_ALPHA / ONE_MINUS_SRC_ALPHA factors.
    gl_begin(&mut ctx, GL_QUADS);
    gl_color(&mut ctx, 0.0, 0.0, 1.0, 0.5);
    gl_vertex(&mut ctx, -1.0, -1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 1.0, -1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 1.0, 1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, -1.0, 1.0, 0.0, 1.0);
    gl_end(&mut ctx);
    // The vertex alpha packs to 128, so the blend runs with
    // src_alpha = 128/255 (not exactly 0.5): out = src·sa + dst·(1−sa).
    // r = (1 − 128/255)·255 = 127, b = 128/255·255 = 128, alpha ≈ 191.
    assert_eq!(
        pixel(&ctx, 16, 16),
        0xBF_7F_00_80,
        "half-alpha blue over red"
    );
}

/// Bind a 2×2 checkerboard (bottom row red/green, top row blue/white) to
/// the context with NEAREST sampling and REPEAT wrap.
fn checkerboard_ctx() -> GlCtx {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_enable(&mut ctx, GL_TEXTURE_2D);
    gl_gen_textures(&mut ctx, 1);
    gl_bind_texture(&mut ctx, GL_TEXTURE_2D, 1);
    gl_tex_parameter_i(&mut ctx, GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    gl_tex_parameter_i(&mut ctx, GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    gl_tex_parameter_i(&mut ctx, GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_REPEAT);
    gl_tex_parameter_i(&mut ctx, GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_REPEAT);
    // Guest rows: row 0 = GL bottom row: red, green; row 1: blue, white.
    let data: [u8; 16] = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    gl_tex_image_2d(
        &mut ctx,
        GL_TEXTURE_2D,
        0,
        GL_RGBA,
        2,
        2,
        0,
        GL_RGBA,
        GL_UNSIGNED_BYTE,
        Some(&data),
    );
    ctx
}

/// Draw a full-viewport quad with the given color and unit texcoords.
fn textured_quad(ctx: &mut GlCtx, color: [f32; 4]) {
    gl_color(ctx, color[0], color[1], color[2], color[3]);
    gl_begin(ctx, GL_QUADS);
    gl_tex_coord(ctx, 0.0, 0.0, 0.0, 1.0);
    gl_vertex(ctx, -1.0, -1.0, 0.0, 1.0);
    gl_tex_coord(ctx, 1.0, 0.0, 0.0, 1.0);
    gl_vertex(ctx, 1.0, -1.0, 0.0, 1.0);
    gl_tex_coord(ctx, 1.0, 1.0, 0.0, 1.0);
    gl_vertex(ctx, 1.0, 1.0, 0.0, 1.0);
    gl_tex_coord(ctx, 0.0, 1.0, 0.0, 1.0);
    gl_vertex(ctx, -1.0, 1.0, 0.0, 1.0);
    gl_end(ctx);
}

#[test]
fn texture_modulate_samples_checkerboard() {
    let mut ctx = checkerboard_ctx();
    // White vertex color: GL_MODULATE passes each texel through.
    textured_quad(&mut ctx, [1.0, 1.0, 1.0, 1.0]);
    assert_eq!(pixel(&ctx, 8, 24), 0xFF_FF_00_00, "bottom-left texel red");
    assert_eq!(
        pixel(&ctx, 24, 24),
        0xFF_00_FF_00,
        "bottom-right texel green"
    );
    assert_eq!(pixel(&ctx, 8, 8), 0xFF_00_00_FF, "top-left texel blue");
    assert_eq!(pixel(&ctx, 24, 8), 0xFF_FF_FF_FF, "top-right texel white");
}

#[test]
fn texture_modulate_multiplies_vertex_color() {
    let mut ctx = checkerboard_ctx();
    // Red vertex color: the green texel modulates to black.
    textured_quad(&mut ctx, [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(
        pixel(&ctx, 24, 24),
        0xFF_00_00_00,
        "green texel × red = black"
    );
    assert_eq!(
        pixel(&ctx, 8, 24),
        0xFF_FF_00_00,
        "red texel × red stays red"
    );
}

#[test]
fn ortho_maps_vertex_to_viewport_center() {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 64, 64);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    gl_ortho(&mut ctx, 0.0, 640.0, 0.0, 480.0, -1.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_color(&mut ctx, 1.0, 1.0, 0.0, 1.0);
    gl_begin(&mut ctx, GL_POINTS);
    gl_vertex(&mut ctx, 320.0, 240.0, 0.0, 1.0);
    gl_end(&mut ctx);
    // NDC (0, 0, 0) → viewport center (32, 32).
    let index = 32_usize.saturating_mul(64).saturating_add(32);
    assert_eq!(
        ctx.backbuffer.get(index).copied().unwrap_or(0),
        0xFF_FF_FF_00
    );
}

#[test]
fn rotate_matrix_spins_around_axis() {
    let m = rotate_matrix(90.0, 0.0, 0.0, 1.0);
    let out = transform_point([1.0, 0.0, 0.0, 1.0], &m);
    assert!(out[0].abs() < 1.0e-5, "x maps to ~0, got {}", out[0]);
    assert!(
        (out[1] - 1.0).abs() < 1.0e-5,
        "x maps to +y, got {}",
        out[1]
    );
    assert!((out[3] - 1.0).abs() < 1.0e-5);
}

#[test]
fn matrix_stack_push_pop_round_trip() {
    let mut ctx = test_ctx();
    gl_translate(&mut ctx, 3.0, 4.0, 0.0);
    gl_push_matrix(&mut ctx);
    gl_translate(&mut ctx, 1.0, 0.0, 0.0);
    let m = ctx.modelview_stack.last().copied().unwrap_or(IDENTITY);
    let p = transform_point([0.0, 0.0, 0.0, 1.0], &m);
    assert!(
        (p[0] - 4.0).abs() < 1.0e-5,
        "stacked translate (4, 4), got {}",
        p[0]
    );
    assert!((p[1] - 4.0).abs() < 1.0e-5);
    gl_pop_matrix(&mut ctx);
    let m2 = ctx.modelview_stack.last().copied().unwrap_or(IDENTITY);
    let p2 = transform_point([0.0, 0.0, 0.0, 1.0], &m2);
    assert!(
        (p2[0] - 3.0).abs() < 1.0e-5,
        "popped back to (3, 4), got {}",
        p2[0]
    );
    assert!((p2[1] - 4.0).abs() < 1.0e-5);
}

#[test]
fn gl_get_error_reports_invalid_enum() {
    let mut ctx = test_ctx();
    gl_matrix_mode(&mut ctx, 0xDEAD);
    assert_eq!(gl_get_error(&mut ctx), GL_INVALID_ENUM);
    assert_eq!(
        gl_get_error(&mut ctx),
        GL_NO_ERROR,
        "error clears after read"
    );
}

#[test]
fn gl_read_pixels_reads_back_gl_rgba() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.1, 0.1, 0.4, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    // GL (0,0) = bottom-left = backbuffer row height-1 (top-down).
    let bytes = gl_read_pixels(&ctx, 0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE);
    assert_eq!(bytes, [26, 26, 102, 255], "bottom-left readback");
}

// ── Stage-2: vertex arrays, VBOs, display lists, lighting ───────────────

use super::arrays as a;
use super::light::{GL_DIFFUSE, GL_LIGHT_MODEL_AMBIENT, GL_POSITION};
use super::lists as l;

/// A host-side stand-in for guest memory: the render layer's `GuestRead`
/// closure reads through this, so a test can point a client array at a
/// byte blob without a real engine.
struct FakeGuest {
    mem: Vec<u8>,
}

impl FakeGuest {
    fn new(bytes: &[u8]) -> Self {
        Self {
            mem: bytes.to_vec(),
        }
    }

    fn read(&self, va: u64, buf: &mut [u8]) -> bool {
        let start = usize::try_from(va).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        let Some(slice) = self.mem.get(start..end) else {
            return false;
        };
        buf.copy_from_slice(slice);
        true
    }
}

fn f32s(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn u16s(values: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Draw the standard right triangle (screen (8,8),(24,8),(8,24)) from a
/// client vertex array; the current color fills in (no color array).
#[test]
fn draw_arrays_pulls_client_vertices() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_color(&mut ctx, 1.0, 1.0, 0.0, 1.0);
    let guest = FakeGuest::new(&f32s(&[-0.5, 0.5, 0.5, 0.5, -0.5, -0.5]));
    a::gl_vertex_pointer(&mut ctx, 2, a::GL_FLOAT, 0, 0);
    a::client_state(&mut ctx, a::GL_VERTEX_ARRAY, true);
    a::gl_draw_arrays(&mut ctx, GL_TRIANGLES, 0, 3, &mut |va, buf| {
        guest.read(va, buf)
    });
    // Centroid (13,13) yellow; outside stays clear.
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_FF_FF_00,
        "client-array triangle centroid"
    );
    assert_eq!(
        pixel(&ctx, 30, 30),
        0xFF_00_00_00,
        "outside the array triangle"
    );
}

/// A quad (two triangles) drawn through a VBO-backed vertex array + an
/// element-buffer index list — the GL 1.5 path Qt uses.
#[test]
fn draw_elements_uses_vbo_and_index_buffer() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_color(&mut ctx, 0.0, 1.0, 1.0, 1.0);
    // Quad NDC (-0.5,-0.5)..(0.5,0.5) → screen (8,24),(24,24),(24,8),(8,8).
    let vertices = f32s(&[-0.5, -0.5, 0.5, -0.5, 0.5, 0.5, -0.5, 0.5]);
    let indices = u16s(&[0, 1, 2, 0, 2, 3]);
    // Data at nonzero guest VAs (0 is GL's NULL upload pointer).
    let mut mem = vec![0_u8; 64];
    mem.extend_from_slice(&vertices);
    mem.extend_from_slice(&indices);
    let guest = FakeGuest::new(&mem);
    a::gl_gen_buffers(&mut ctx, 2);
    a::gl_bind_buffer(&mut ctx, a::GL_ARRAY_BUFFER, 1);
    a::gl_buffer_data(
        &mut ctx,
        a::GL_ARRAY_BUFFER,
        32,
        64,
        a::GL_STATIC_DRAW,
        &mut |va, buf| guest.read(va, buf),
    );
    a::gl_bind_buffer(&mut ctx, a::GL_ELEMENT_ARRAY_BUFFER, 2);
    a::gl_buffer_data(
        &mut ctx,
        a::GL_ELEMENT_ARRAY_BUFFER,
        12,
        64 + 32,
        a::GL_STATIC_DRAW,
        &mut |va, buf| guest.read(va, buf),
    );
    a::gl_vertex_pointer(&mut ctx, 2, a::GL_FLOAT, 0, 0); // offset 0 into the VBO
    a::client_state(&mut ctx, a::GL_VERTEX_ARRAY, true);
    a::gl_draw_elements(
        &mut ctx,
        GL_TRIANGLES,
        6,
        a::GL_UNSIGNED_SHORT,
        0,
        &mut |va, buf| guest.read(va, buf),
    );
    assert_eq!(pixel(&ctx, 16, 16), 0xFF_00_FF_FF, "VBO quad center");
    assert_eq!(pixel(&ctx, 4, 28), 0xFF_00_00_00, "outside the VBO quad");
    let (ab, eb) = a::buffer_bindings(&ctx);
    assert_eq!((ab, eb), (1, 2), "bindings round-trip");
}

/// A compiled display list replays identically to the direct draw.
#[test]
fn display_list_replays_like_direct_draw() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    // Compile a red triangle into list 1.
    l::gl_new_list(&mut ctx, 1, l::GL_COMPILE);
    gl_color(&mut ctx, 1.0, 0.0, 0.0, 1.0);
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    l::gl_end_list(&mut ctx);
    assert!(l::gl_is_list(&ctx, 1), "list 1 exists");
    l::gl_call_list(&mut ctx, 1, &mut |_, _| false);
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_FF_00_00,
        "replayed triangle centroid"
    );
    // GL_COMPILE must NOT have drawn during capture: the centroid was still
    // the clear color before the call... clear again and verify the direct
    // path draws the same pixel.
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_color(&mut ctx, 1.0, 0.0, 0.0, 1.0);
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_FF_00_00,
        "direct triangle centroid matches"
    );
}

/// A positional light makes a +Z-facing quad brighter near the light than
/// far from it (per-vertex Gouraud through the interpolated lit color).
#[test]
fn lighting_gradates_across_a_quad() {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 64, 64);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    gl_ortho(&mut ctx, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_enable(&mut ctx, GL_LIGHTING);
    gl_enable(&mut ctx, GL_LIGHT0);
    // Positional light close above the quad's top-right corner.
    ctx.light_fv(GL_LIGHT0, GL_POSITION, &[0.7, 0.7, 0.6, 1.0]);
    ctx.material_fv(GL_FRONT_AND_BACK, GL_DIFFUSE, &[1.0, 1.0, 1.0, 1.0]);
    ctx.light_model_fv(GL_LIGHT_MODEL_AMBIENT, &[0.0, 0.0, 0.0, 1.0]);
    gl_normal(&mut ctx, 0.0, 0.0, 1.0);
    gl_begin(&mut ctx, GL_QUADS);
    gl_vertex(&mut ctx, -0.8, -0.8, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.8, -0.8, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.8, 0.8, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.8, 0.8, 0.0, 1.0);
    gl_end(&mut ctx);
    // NDC (0.4, 0.4) → screen x = 0.7*32 = 22.4, y = (1-0.4)*32 = 19.2 → the
    // near-light corner; (-0.5, -0.5) → (8, 48) the far corner.
    let near = pixel(&ctx, 22, 19);
    let far = pixel(&ctx, 8, 48);
    let near_r = (near >> 16) & 0xFF;
    let far_r = (far >> 16) & 0xFF;
    assert!(
        near_r > far_r + 30,
        "lit gradient near={near_r} far={far_r}"
    );
}

/// A directional light from +Z lights a front-facing triangle but not a
/// back-facing one (N·L sign flips).
#[test]
fn lighting_front_facing_brighter_than_back_facing() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_enable(&mut ctx, GL_LIGHTING);
    gl_enable(&mut ctx, GL_LIGHT0);
    // Directional light along +Z: L = (0,0,1).
    ctx.light_fv(GL_LIGHT0, GL_POSITION, &[0.0, 0.0, 1.0, 0.0]);
    ctx.material_fv(GL_FRONT_AND_BACK, GL_DIFFUSE, &[1.0, 1.0, 1.0, 1.0]);
    ctx.light_model_fv(GL_LIGHT_MODEL_AMBIENT, &[0.0, 0.0, 0.0, 1.0]);
    let tri = |ctx: &mut GlCtx, nx: f32, ny: f32, nz: f32| {
        gl_normal(ctx, nx, ny, nz);
        gl_begin(ctx, GL_TRIANGLES);
        gl_vertex(ctx, -0.5, 0.5, 0.0, 1.0);
        gl_vertex(ctx, 0.5, 0.5, 0.0, 1.0);
        gl_vertex(ctx, -0.5, -0.5, 0.0, 1.0);
        gl_end(ctx);
    };
    tri(&mut ctx, 0.0, 0.0, 1.0); // front: N·L = 1
    assert_eq!(pixel(&ctx, 13, 13), 0xFF_FF_FF_FF, "front-facing fully lit");
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    tri(&mut ctx, 0.0, 0.0, -1.0); // back: N·L = -1 → black
    assert_eq!(pixel(&ctx, 13, 13), 0xFF_00_00_00, "back-facing black");
}

/// GL_FLAT shades the whole primitive with the first vertex's color, so the
/// centroid differs from GL_SMOOTH's interpolation.
#[test]
fn flat_vs_smooth_shading_centroids_differ() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    let tri = |ctx: &mut GlCtx| {
        gl_begin(ctx, GL_TRIANGLES);
        gl_color(ctx, 1.0, 0.0, 0.0, 1.0); // first vertex red
        gl_vertex(ctx, -0.5, 0.5, 0.0, 1.0);
        gl_color(ctx, 0.0, 0.0, 1.0, 1.0); // others blue
        gl_vertex(ctx, 0.5, 0.5, 0.0, 1.0);
        gl_vertex(ctx, -0.5, -0.5, 0.0, 1.0);
        gl_end(ctx);
    };
    tri(&mut ctx); // GL_SMOOTH (default)
    let smooth = pixel(&ctx, 13, 13);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    gl_shade_model(&mut ctx, GL_FLAT);
    tri(&mut ctx);
    let flat = pixel(&ctx, 13, 13);
    assert_eq!(flat, 0xFF_FF_00_00, "GL_FLAT uses the first vertex color");
    assert_ne!(smooth, flat, "GL_SMOOTH interpolates, GL_FLAT does not");
}

/// A list replayed under two modelview offsets draws both instances (the
/// micro's `glCallList` twice + `glTranslatef` between).
#[test]
fn display_list_replays_at_modelview_offsets() {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 640, 480);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    gl_ortho(&mut ctx, 0.0, 640.0, 0.0, 480.0, -1.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    gl_load_identity(&mut ctx);
    gl_clear_color(&mut ctx, 0.1, 0.1, 0.4, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    l::gl_new_list(&mut ctx, 1, l::GL_COMPILE);
    gl_color(&mut ctx, 0.0, 1.0, 1.0, 1.0);
    gl_begin(&mut ctx, GL_QUADS);
    gl_vertex(&mut ctx, 320.0, 330.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 400.0, 330.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 400.0, 410.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 320.0, 410.0, 0.0, 1.0);
    gl_end(&mut ctx);
    l::gl_end_list(&mut ctx);
    l::gl_call_list(&mut ctx, 1, &mut |_, _| false);
    gl_translate(&mut ctx, -90.0, 0.0, 0.0);
    l::gl_call_list(&mut ctx, 1, &mut |_, _| false);
    // GL (380, 370) → screen row 480-1-370 = 109, col 380 (instance 1);
    // GL (270, 370) → col 270 (instance 2); GL (210, 370) → gap.
    let at = |x: usize| {
        ctx.backbuffer
            .get(109_usize.saturating_mul(640).saturating_add(x))
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(at(380), 0xFF_00_FF_FF, "list instance 1");
    assert_eq!(
        at(270),
        0xFF_00_FF_FF,
        "list instance 2 under the translate"
    );
    assert_eq!(
        at(210),
        0xFF_1A_1A_66,
        "gap between instances is the clear color"
    );
}

// ── GLSL shader pipeline ────────────────────────────────────────────────

/// Compile + link a minimal pass-through VS / constant-red FS program and
/// return the program id (active).
fn red_program(ctx: &mut GlCtx) -> u32 {
    let vs = glsl::gl_create_shader(ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(
        ctx,
        vs,
        "attribute vec4 gl_Vertex; void main() { gl_Position = gl_Vertex; }",
    );
    glsl::gl_compile_shader(ctx, vs);
    assert_eq!(glsl::gl_shader_compile_status(ctx, vs), 1, "VS compiles");
    let fs = glsl::gl_create_shader(ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(
        ctx,
        fs,
        "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }",
    );
    glsl::gl_compile_shader(ctx, fs);
    assert_eq!(glsl::gl_shader_compile_status(ctx, fs), 1, "FS compiles");
    let prog = glsl::gl_create_program(ctx);
    glsl::gl_attach_shader(ctx, prog, vs);
    glsl::gl_attach_shader(ctx, prog, fs);
    glsl::gl_link_program(ctx, prog);
    assert_eq!(glsl::gl_program_link_status(ctx, prog), 1, "link ok");
    glsl::gl_use_program(ctx, prog);
    prog
}

#[test]
fn shader_compile_link_and_use_vertex_program() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    red_program(&mut ctx);
    // The same triangle as the fixed-function color test: vertex colors are
    // irrelevant — the FS returns constant red.
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_color(&mut ctx, 0.0, 1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_color(&mut ctx, 0.0, 0.0, 1.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_color(&mut ctx, 1.0, 1.0, 1.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(pixel(&ctx, 13, 13), 0xFF_FF_00_00, "FS constant red");
    assert_eq!(pixel(&ctx, 9, 9), 0xFF_FF_00_00, "near-vertex red");
    assert_eq!(pixel(&ctx, 30, 30), 0xFF_00_00_00, "outside stays clear");
}

#[test]
fn fragment_shader_overrides_fixed_function_color() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    let vs = glsl::gl_create_shader(&mut ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        vs,
        "attribute vec4 gl_Vertex; void main() { gl_Position = gl_Vertex; }",
    );
    glsl::gl_compile_shader(&mut ctx, vs);
    let fs = glsl::gl_create_shader(&mut ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        fs,
        "void main() { gl_FragColor = vec4(0.0, 0.0, 1.0, 1.0); }",
    );
    glsl::gl_compile_shader(&mut ctx, fs);
    let prog = glsl::gl_create_program(&mut ctx);
    glsl::gl_attach_shader(&mut ctx, prog, vs);
    glsl::gl_attach_shader(&mut ctx, prog, fs);
    glsl::gl_link_program(&mut ctx, prog);
    glsl::gl_use_program(&mut ctx, prog);
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_color(&mut ctx, 1.0, 0.0, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_00_00_FF,
        "FS blue overrides the red vertex color"
    );
}

#[test]
fn varying_interpolates_across_triangle() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    // VS maps NDC xy → uv (0..1); FS colors by the interpolated varying.
    let vs = glsl::gl_create_shader(&mut ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        vs,
        "attribute vec4 gl_Vertex; varying vec2 v_uv; \
         void main() { v_uv = gl_Vertex.xy * 0.5 + 0.5; gl_Position = gl_Vertex; }",
    );
    glsl::gl_compile_shader(&mut ctx, vs);
    let fs = glsl::gl_create_shader(&mut ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        fs,
        "varying vec2 v_uv; void main() { gl_FragColor = vec4(v_uv, 0.0, 1.0); }",
    );
    glsl::gl_compile_shader(&mut ctx, fs);
    let prog = glsl::gl_create_program(&mut ctx);
    glsl::gl_attach_shader(&mut ctx, prog, vs);
    glsl::gl_attach_shader(&mut ctx, prog, fs);
    glsl::gl_link_program(&mut ctx, prog);
    glsl::gl_use_program(&mut ctx, prog);
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0); // uv (0.25, 0.75)
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0); // uv (0.75, 0.75)
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0); // uv (0.25, 0.25)
    gl_end(&mut ctx);
    // Centroid pixel (13,13), barycentric (80,88,88)/256 → uv = (108,148)/256
    // → r = 108, g = 148·255/256 = 147.4 → 147 (0x93); b = 0; a = 1.
    let c = pixel(&ctx, 13, 13);
    assert_eq!(
        c & 0xFF_FF_FF_00,
        0xFF_6C_93_00,
        "centroid uv = barycentric mix (got 0x{c:08x})"
    );
    // Near the (0.25,0.25) vertex, both channels must be BELOW the centroid
    // (the uv gradient runs toward the clear-dark corner) and covered.
    let near = pixel(&ctx, 9, 21);
    let near_r = (near >> 16) & 0xFF;
    let near_g = (near >> 8) & 0xFF;
    assert!(
        near_r < 0x6C && near_g < 0x93,
        "uv-weighted pixel near the (0.25,0.25) vertex is darker than the \
         centroid (got 0x{near:08x})"
    );
    assert_ne!(near, 0xFF_00_00_00, "pixel is covered by the triangle");
}

#[test]
fn texture2d_in_fragment_shader_samples_texel() {
    let mut ctx = checkerboard_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    let vs = glsl::gl_create_shader(&mut ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        vs,
        "attribute vec4 gl_Vertex; void main() { gl_Position = gl_Vertex; }",
    );
    glsl::gl_compile_shader(&mut ctx, vs);
    let fs = glsl::gl_create_shader(&mut ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        fs,
        "uniform sampler2D u_tex; void main() { \
         gl_FragColor = texture2D(u_tex, vec2(0.25, 0.25)); }",
    );
    glsl::gl_compile_shader(&mut ctx, fs);
    let prog = glsl::gl_create_program(&mut ctx);
    glsl::gl_attach_shader(&mut ctx, prog, vs);
    glsl::gl_attach_shader(&mut ctx, prog, fs);
    glsl::gl_link_program(&mut ctx, prog);
    glsl::gl_use_program(&mut ctx, prog);
    let loc = glsl::gl_get_uniform_location(&ctx, prog, "u_tex");
    assert!(loc >= 0, "sampler uniform resolves");
    glsl::gl_uniform_set(&mut ctx, loc, glsl::GlslVal::Sampler(0));
    // Full-viewport quad; the FS samples (0.25,0.25) → bottom-left texel red.
    gl_begin(&mut ctx, GL_QUADS);
    gl_vertex(&mut ctx, -1.0, -1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 1.0, -1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, 1.0, 1.0, 0.0, 1.0);
    gl_vertex(&mut ctx, -1.0, 1.0, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(
        pixel(&ctx, 16, 16),
        0xFF_FF_00_00,
        "FS texture2D returns the sampled texel"
    );
}

#[test]
fn shader_compile_error_reports_log() {
    let mut ctx = test_ctx();
    let vs = glsl::gl_create_shader(&mut ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(&mut ctx, vs, "void main() { gl_Position = gl_Vertex; }");
    glsl::gl_compile_shader(&mut ctx, vs);
    assert_eq!(glsl::gl_shader_compile_status(&ctx, vs), 1, "VS compiles");
    let bad = glsl::gl_create_shader(&mut ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(&mut ctx, bad, "void main() { gl_FragColor = nope; }");
    glsl::gl_compile_shader(&mut ctx, bad);
    assert_eq!(
        glsl::gl_shader_compile_status(&ctx, bad),
        0,
        "undeclared identifier fails compile"
    );
    let log = glsl::gl_shader_info_log(&ctx, bad);
    assert!(!log.is_empty(), "info log reports the error");
    assert!(
        log.contains("unknown identifier") || log.contains("line"),
        "log mentions the failure (got: {log})"
    );
}

#[test]
fn uniform_matrix4fv_affects_position() {
    let mut ctx = test_ctx();
    gl_clear_color(&mut ctx, 0.0, 0.0, 0.0, 1.0);
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    let vs = glsl::gl_create_shader(&mut ctx, glsl::GL_VERTEX_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        vs,
        "uniform mat4 u_mvp; attribute vec4 gl_Vertex; \
         void main() { gl_Position = u_mvp * gl_Vertex; }",
    );
    glsl::gl_compile_shader(&mut ctx, vs);
    let fs = glsl::gl_create_shader(&mut ctx, glsl::GL_FRAGMENT_SHADER);
    glsl::gl_shader_source(
        &mut ctx,
        fs,
        "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }",
    );
    glsl::gl_compile_shader(&mut ctx, fs);
    let prog = glsl::gl_create_program(&mut ctx);
    glsl::gl_attach_shader(&mut ctx, prog, vs);
    glsl::gl_attach_shader(&mut ctx, prog, fs);
    glsl::gl_link_program(&mut ctx, prog);
    glsl::gl_use_program(&mut ctx, prog);
    let loc = glsl::gl_get_uniform_location(&ctx, prog, "u_mvp");
    assert!(loc >= 0, "mvp uniform resolves");
    // Identity first: the triangle sits at the ortho center.
    let identity = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    glsl::gl_uniform_set(&mut ctx, loc, glsl::GlslVal::Mat4(identity));
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(
        pixel(&ctx, 13, 13),
        0xFF_FF_00_00,
        "identity mvp leaves the triangle centered"
    );
    gl_clear(&mut ctx, GL_COLOR_BUFFER_BIT);
    // Translate +0.5 NDC in x (column-major): the triangle moves right 8px.
    let translated = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.5, 0.0, 0.0, 1.0,
    ];
    glsl::gl_uniform_set(&mut ctx, loc, glsl::GlslVal::Mat4(translated));
    gl_begin(&mut ctx, GL_TRIANGLES);
    gl_vertex(&mut ctx, -0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, 0.5, 0.5, 0.0, 1.0);
    gl_vertex(&mut ctx, -0.5, -0.5, 0.0, 1.0);
    gl_end(&mut ctx);
    assert_eq!(
        pixel(&ctx, 21, 13),
        0xFF_FF_00_00,
        "translated mvp moves the triangle right"
    );
    assert_eq!(
        pixel(&ctx, 5, 13),
        0xFF_00_00_00,
        "the original triangle position is now clear"
    );
}

/// The micro's quad-center readback must survive a WM_SIZE: after the
/// backbuffer follows the window's new client size (what `resize_window`
/// now triggers via `gl_ensure_framebuffer`), the same world-space quad
/// maps to the new viewport center and the readback lands on it.
///
/// Regression for gl_quad's exit-110 on resize — before the fix nothing
/// resized the backbuffer with the window, so the first paint after a
/// resize drew and read back against a stale-sized buffer.
#[test]
fn resize_keeps_quad_center_readback_red() {
    let mut ctx = GlCtx::new();
    gl_ensure_framebuffer(&mut ctx, 640, 480);
    gl_matrix_mode(&mut ctx, GL_PROJECTION);
    gl_ortho(&mut ctx, 0.0, 640.0, 0.0, 480.0, -1.0, 1.0);
    gl_matrix_mode(&mut ctx, GL_MODELVIEW);
    gl_clear_color(&mut ctx, 0.1, 0.1, 0.4, 1.0);

    // The micro's red quad at world [240,400]×[180,300] (center (320,240)).
    let draw_quad = |ctx: &mut GlCtx| {
        gl_clear(ctx, GL_COLOR_BUFFER_BIT);
        gl_color(ctx, 1.0, 0.0, 0.0, 1.0);
        gl_begin(ctx, GL_QUADS);
        gl_vertex(ctx, 240.0, 180.0, 0.0, 1.0);
        gl_vertex(ctx, 400.0, 180.0, 0.0, 1.0);
        gl_vertex(ctx, 400.0, 300.0, 0.0, 1.0);
        gl_vertex(ctx, 240.0, 300.0, 0.0, 1.0);
        gl_end(ctx);
    };

    gl_viewport(&mut ctx, 0, 0, 640, 480);
    draw_quad(&mut ctx);
    let px = gl_read_pixels(&ctx, 320, 240, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE);
    assert_eq!(&px[..3], &[255, 0, 0], "center red at 640x480");

    // WM_SIZE to 800×600: the backbuffer follows the window.
    gl_ensure_framebuffer(&mut ctx, 800, 600);
    gl_viewport(&mut ctx, 0, 0, 800, 600);
    draw_quad(&mut ctx);
    let px = gl_read_pixels(&ctx, 400, 300, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE);
    assert_eq!(&px[..3], &[255, 0, 0], "center red after resize to 800x600");
}
