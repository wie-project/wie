use super::blend::{blend_fragment, depth_test};
use super::{
    D3DBLEND_INVSRCALPHA, D3DBLEND_SRCALPHA, D3DBLENDOP_ADD, D3DBLENDOP_REVSUBTRACT,
    D3DBLENDOP_SUBTRACT, D3DCMP_ALWAYS, D3DCMP_EQUAL, D3DCMP_GREATER, D3DCMP_GREATEREQUAL,
    D3DCMP_LESS, D3DCMP_LESSEQUAL, D3DCMP_NEVER, D3DCMP_NOTEQUAL, D3DFVF_DIFFUSE, D3DFVF_NORMAL,
    D3DFVF_SPECULAR, D3DFVF_XYZ, D3DFVF_XYZRHW, GuestVertex, IDENTITY, PsFragmentInput, PsProgram,
    ScreenVertex, Viewport, clip_to_screen, draw_triangle, is_top_or_left_edge, mat4_mul,
    parse_fvf, parse_vertex, pixel_shader_alpha_to_u8, pixel_shader_color_to_0rgb,
    rasterize_triangle, run_pixel_shader, transform_point,
};
use crate::d3d9_shader::{
    D3DSPDM_NONE, D3DSPDM_SATURATE, D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG,
    D3DSPSM_COMP, D3DSPSM_NEG, D3DSPSM_NONE, D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2,
    D3DSPSM_X2NEG, Operand, PsInstruction, PsOp, RegType,
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
    assert_eq!(super::sample::sample_texture(&wrap, 0.9, 0.1), 3);
    assert_eq!(super::sample::sample_texture(&wrap, -0.1, 0.0), 3);
    // Clamp: out-of-range u clamps to the edge texels.
    let clamp = stage(texels, super::D3DTADDRESS_CLAMP, super::D3DTADDRESS_CLAMP);
    assert_eq!(
        super::sample::sample_texture(&clamp, 2.0, 0.5),
        (2 << 8) | 3
    );
    assert_eq!(super::sample::sample_texture(&clamp, -0.5, 2.0), 3 << 8);
}

#[test]
fn color_op_evaluation() {
    let texel = 0xFF_80_40_20_u32; // r=0x80 g=0x40 b=0x20
    let diffuse = 0xFF_10_20_40_u32; // r=0x10 g=0x20 b=0x40
    // SELECTARG1 → the texel RGB.
    assert_eq!(
        super::sample::eval_color_op(super::D3DTOP_SELECTARG1, texel, diffuse),
        0x00_80_40_20
    );
    // SELECTARG2 → the diffuse RGB.
    assert_eq!(
        super::sample::eval_color_op(super::D3DTOP_SELECTARG2, texel, diffuse),
        0x00_10_20_40
    );
    // MODULATE: per-channel product >> 8.
    assert_eq!(
        super::sample::eval_color_op(super::D3DTOP_MODULATE, texel, diffuse),
        ((0x80_u32 * 0x10) >> 8) << 16 | ((0x40_u32 * 0x20) >> 8) << 8 | ((0x20_u32 * 0x40) >> 8)
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
    // w=0 (on the near-plane) → rejected.
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

// ── pixel-shader interpreter tests ─────────────────────────────────

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
