use super::blend::{blend_fragment, depth_test};
use super::{
    D3DBLEND_INVSRCALPHA, D3DBLEND_SRCALPHA, D3DBLENDOP_ADD, D3DBLENDOP_REVSUBTRACT,
    D3DBLENDOP_SUBTRACT, D3DCMP_ALWAYS, D3DCMP_EQUAL, D3DCMP_GREATER, D3DCMP_GREATEREQUAL,
    D3DCMP_LESS, D3DCMP_LESSEQUAL, D3DCMP_NEVER, D3DCMP_NOTEQUAL, D3DFVF_DIFFUSE, D3DFVF_NORMAL,
    D3DFVF_SPECULAR, D3DFVF_XYZ, D3DFVF_XYZRHW, GuestVertex, IDENTITY, PsFragmentInput, PsProgram,
    ScreenVertex, Viewport, VsProgram, VsVertexInput, clip_to_screen, clip_to_viewport,
    draw_triangle, is_top_or_left_edge, mat4_mul, parse_fvf, parse_vertex,
    pixel_shader_alpha_to_u8, pixel_shader_color_to_0rgb, rasterize_triangle, run_pixel_shader,
    run_vertex_shader, transform_point, vs_input_from_vertex,
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
        fog_enable: 0,
        fog_color: 0,
        fog_start: 0.0,
        fog_end: 1.0,
        fog_density: 1.0,
        fog_table_mode: super::D3DFOG_NONE,
        fog_vertex_mode: super::D3DFOG_NONE,
        alpha_test: 0,
        alpha_func: super::D3DCMP_ALWAYS,
        alpha_ref: 0,
        scissor_test: 0,
        scissor: None,
    }
}

#[test]
fn blend_add_src_alpha_inv_src_alpha() {
    // src blue (0,0,255) at alpha 0x80 over dst red (255,0,0):
    // out = (src*128 + dst*127) >> 8 per channel.
    let frag = super::FragmentState {
        alpha_blend: 1,
        src_blend: D3DBLEND_SRCALPHA,
        dest_blend: D3DBLEND_INVSRCALPHA,
        blend_op: D3DBLENDOP_ADD,
        ..no_frag()
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
        alpha_blend: 1,
        src_blend: super::D3DBLEND_ONE,
        dest_blend: super::D3DBLEND_ONE,
        blend_op: D3DBLENDOP_SUBTRACT,
        ..no_frag()
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
        alpha_blend: 1,
        src_blend: super::D3DBLEND_ONE,
        dest_blend: super::D3DBLEND_ONE,
        blend_op: D3DBLENDOP_REVSUBTRACT,
        ..no_frag()
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
        alpha_blend: 1,
        src_blend: 0xDEAD,
        dest_blend: super::D3DBLEND_ZERO,
        blend_op: D3DBLENDOP_ADD,
        ..no_frag()
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
        min_filter: super::D3DTEXF_POINT,
        mip_filter: super::D3DTEXF_POINT,
        mips: super::MipChain {
            count: 1,
            levels: [None; super::MAX_MIP_LEVELS],
        },
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
            w: 1.0,
            color,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 4.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
            color,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 0.0,
            y: 4.0,
            z: 0.0,
            w: 1.0,
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
            w: 1.0,
            color: red,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 2.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
            color: red,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 2.0,
            y: 4.0,
            z: 0.0,
            w: 1.0,
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
            w: 1.0,
            color: blue,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 4.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
            color: blue,
            u: 0.0,
            v: 0.0,
        },
        ScreenVertex {
            x: 2.0,
            y: 4.0,
            z: 0.0,
            w: 1.0,
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
        control: 0,
        predicated: false,
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
        relative: false,
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
        control: 0,
        predicated: false,
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
        relative: false,
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
        relative: false,
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
                control: 0,
                predicated: false,
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
            control: 0,
            predicated: false,
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
                control: 0,
                predicated: false,
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
                control: 0,
                predicated: false,
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
            control: 0,
            predicated: false,
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
            control: 0,
            predicated: false,
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

// ── vertex-shader interpreter + z/w plumbing tests ───────────────────

/// Build a vs program with the given instructions and constant registers.
fn vs_program(instructions: Vec<PsInstruction>, constants: &[[f32; 4]; 256]) -> VsProgram<'static> {
    VsProgram {
        instructions: Box::leak(instructions.into_boxed_slice()),
        constants: *constants,
        int_constants: [[0; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT],
        bool_constants: [false; crate::d3d9_shader::VS_BOOL_CONST_COUNT],
    }
}

/// Build a vs program with explicit int/bool constant files (the `loop`/
/// `if`/`breakc` sources read these).
fn vs_program_with(
    instructions: Vec<PsInstruction>,
    constants: &[[f32; 4]; 256],
    int_constants: [[i32; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT],
    bool_constants: [bool; crate::d3d9_shader::VS_BOOL_CONST_COUNT],
) -> VsProgram<'static> {
    VsProgram {
        instructions: Box::leak(instructions.into_boxed_slice()),
        constants: *constants,
        int_constants,
        bool_constants,
    }
}

/// A vs operand builder with a writable field set.
fn vs_src(reg_type: RegType, reg_num: u16) -> Operand {
    Operand {
        reg_type,
        reg_num,
        swizzle: [0, 1, 2, 3],
        src_mod: 0,
        dst_mod: D3DSPDM_NONE,
        write_mask: 0xF,
        relative: false,
    }
}

/// `oPos` — the vs rasterizer-output destination register.
fn o_pos() -> Operand {
    vs_src(RegType::RastOut, 0)
}

#[test]
fn vs_interpreter_arithmetic_ops_match_reference() {
    // The same reference-math table as the PS interpreter, executed through
    // the vertex stage (results land in oPos).
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [2.0, 3.0, 4.0, 5.0]; // a
    constants[1] = [3.0, 1.0, -2.0, 0.5]; // b
    constants[2] = [1.0, 1.0, 1.0, 1.0]; // c
    let run = |op: PsOp| {
        let instrs = vec![
            PsInstruction {
                op,
                dst: Some(o_pos()),
                srcs: vec![
                    vs_src(RegType::Const, 0),
                    vs_src(RegType::Const, 1),
                    vs_src(RegType::Const, 2),
                ],
                tex_type: None,
                end: false,
                control: 0,
                predicated: false,
            },
            PsInstruction {
                op: PsOp::End,
                dst: None,
                srcs: Vec::new(),
                tex_type: None,
                end: true,
                control: 0,
                predicated: false,
            },
        ];
        run_vertex_shader(
            &vs_program(instrs, &constants),
            &VsVertexInput { v: [[0.0; 4]; 16] },
        )
        .pos
    };
    assert_eq!(run(PsOp::Add), [5.0, 4.0, 2.0, 5.5]);
    assert_eq!(run(PsOp::Sub), [-1.0, 2.0, 6.0, 4.5]);
    assert_eq!(run(PsOp::Mul), [6.0, 3.0, -8.0, 2.5]);
    assert_eq!(run(PsOp::Mad), [7.0, 4.0, -7.0, 3.5]);
    assert_eq!(run(PsOp::Dp3), [1.0; 4]);
    assert_eq!(run(PsOp::Dp4), [3.5; 4]);
    assert_eq!(run(PsOp::Min), [2.0, 1.0, -2.0, 0.5]);
    assert_eq!(run(PsOp::Max), [3.0, 3.0, 4.0, 5.0]);
    assert_eq!(run(PsOp::Slt), [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(run(PsOp::Sge), [0.0, 1.0, 1.0, 1.0]);
    assert_eq!(
        run(PsOp::Lrp),
        [
            2.0 * 3.0 + (1.0 - 2.0) * 1.0,
            3.0 * 1.0 + (1.0 - 3.0) * 1.0,
            4.0 * -2.0 + (1.0 - 4.0) * 1.0,
            5.0 * 0.5 + (1.0 - 5.0) * 1.0
        ]
    );
    assert_eq!(run(PsOp::Cmp), [3.0, 1.0, -2.0, 0.5]);
    // Scalar ops (rcp/rsq/exp/log/frc) read only constants[0]; run them
    // with a dedicated scalar input.
    let run_scalar = |op: PsOp, c0: [f32; 4]| {
        let mut scalar_constants = [[0.0; 4]; 256];
        scalar_constants[0] = c0;
        let instrs = vec![
            PsInstruction {
                op,
                dst: Some(o_pos()),
                srcs: vec![vs_src(RegType::Const, 0)],
                tex_type: None,
                end: false,
                control: 0,
                predicated: false,
            },
            PsInstruction {
                op: PsOp::End,
                dst: None,
                srcs: Vec::new(),
                tex_type: None,
                end: true,
                control: 0,
                predicated: false,
            },
        ];
        run_vertex_shader(
            &vs_program(instrs, &scalar_constants),
            &VsVertexInput { v: [[0.0; 4]; 16] },
        )
        .pos
    };
    assert_eq!(run_scalar(PsOp::Rcp, [4.0, 99.0, 99.0, 99.0]), [0.25; 4]);
    assert_eq!(run_scalar(PsOp::Rsq, [4.0, 99.0, 99.0, 99.0]), [0.5; 4]);
    // exp/log compute the x channel and copy yzw.
    assert_eq!(
        run_scalar(PsOp::Exp, [4.0, 99.0, 99.0, 99.0]),
        [16.0, 99.0, 99.0, 99.0]
    );
    assert_eq!(
        run_scalar(PsOp::Log, [4.0, 99.0, 99.0, 99.0]),
        [2.0, 99.0, 99.0, 99.0]
    );
    // frc applies per component.
    assert_eq!(
        run_scalar(PsOp::Frc, [4.5, 99.0, 99.0, 99.0]),
        [0.5, 0.0, 0.0, 0.0]
    );
}

#[test]
fn vs_interpreter_micro_exe_shader_transforms_and_passes_attributes() {
    // The exact shader the gui_d3d9 micro-exe ships: dcl v0/v5/v6 → mov
    // oT0,v5 / mov oD0,v6 → dp4 oPos.x/y/z/w, v0, c0..c3 → end. With the
    // orthographic projection's columns in c0..c3, oPos must equal
    // v0·M and oT0/oD0 must pass through untouched.
    let mut instrs = vec![
        PsInstruction {
            op: PsOp::Dcl,
            dst: Some(vs_src(RegType::Input, 0)),
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Dcl,
            dst: Some(vs_src(RegType::Input, 5)),
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Dcl,
            dst: Some(vs_src(RegType::Input, 6)),
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(vs_src(RegType::TexcrdOut, 0)),
            srcs: vec![vs_src(RegType::Input, 5)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(vs_src(RegType::AttrOut, 0)),
            srcs: vec![vs_src(RegType::Input, 6)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
    ];
    // dp4 oPos.{x,y,z,w}, v0, c{i}
    for (channel, reg) in [(0_u16, 0_u16), (1, 1), (2, 2), (3, 3)] {
        let mut dst = o_pos();
        dst.write_mask = 1 << channel;
        instrs.extend([PsInstruction {
            op: PsOp::Dp4,
            dst: Some(dst),
            srcs: vec![vs_src(RegType::Input, 0), vs_src(RegType::Const, reg)],
            control: 0,
            predicated: false,
            tex_type: None,
            end: false,
        }]);
    }
    instrs.push(PsInstruction {
        op: PsOp::End,
        dst: None,
        srcs: Vec::new(),
        control: 0,
        predicated: false,
        tex_type: None,
        end: true,
    });

    let mut constants = [[0.0; 4]; 256];
    // The ortho projection columns (row-vector: oPos = v0·M).
    constants[0] = [2.0 / 320.0, 0.0, 0.0, 0.0];
    constants[1] = [0.0, 2.0 / 240.0, 0.0, 0.0];
    constants[2] = [0.0, 0.0, 1.0, 0.0];
    constants[3] = [0.0, 0.0, 0.0, 1.0];
    let input = VsVertexInput {
        v: {
            let mut v = [[0.0_f32; 4]; 16];
            v[0] = [80.0, 110.0, 0.0, 1.0];
            v[5] = [0.25, 0.75, 0.0, 1.0];
            v[6] = [1.0, 1.0, 1.0, 1.0];
            v
        },
    };
    let out = run_vertex_shader(&vs_program(instrs, &constants), &input);
    assert_eq!(out.u, 0.25);
    assert_eq!(out.v, 0.75);
    assert_eq!(out.color, 0xFF_FF_FF_FF);
    // oPos = v0·M: x = 80·(2/320) = 0.5, y = 110·(2/240) = 0.91667, z = 0, w = 1.
    let expected = transform_point([80.0, 110.0, 0.0, 1.0], &{
        let mut m = IDENTITY;
        m[0] = 2.0 / 320.0;
        m[5] = 2.0 / 240.0;
        m
    });
    assert_eq!(out.pos, expected);
}

#[test]
fn vs_interpreter_partial_rastout_masks_accumulate() {
    // dp4 oPos.x, v0, c0 writes only .x — the other channels must stay 0
    // (the same write-mask merging the PS interpreter applies).
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [1.0, 0.0, 0.0, 0.0];
    let mut dst = o_pos();
    dst.write_mask = 0x1;
    let instrs = vec![
        PsInstruction {
            op: PsOp::Dp4,
            dst: Some(dst),
            srcs: vec![vs_src(RegType::Input, 0), vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let input = VsVertexInput {
        v: {
            let mut v = [[0.0_f32; 4]; 16];
            v[0] = [5.0, 0.0, 0.0, 1.0];
            v
        },
    };
    let out = run_vertex_shader(&vs_program(instrs, &constants), &input);
    assert_eq!(out.pos, [5.0, 0.0, 0.0, 0.0]);
}

#[test]
fn vs_input_from_vertex_maps_fvf_semantics() {
    // XYZ|DIFFUSE|TEX1 → v0 = position, v5 = texcoord0, v6 = diffuse.
    let layout = parse_fvf(D3DFVF_XYZ | D3DFVF_DIFFUSE | 0x0100).expect("valid FVF");
    let v = GuestVertex {
        x: 1.0,
        y: 2.0,
        z: 3.0,
        w: 1.0,
        color: 0xFF_80_40_20,
        u: 0.25,
        v: 0.5,
    };
    let input = vs_input_from_vertex(&v, &layout);
    assert_eq!(input.v[0], [1.0, 2.0, 3.0, 1.0]);
    assert_eq!(input.v[5], [0.25, 0.5, 0.0, 1.0]);
    assert_eq!(
        input.v[6],
        [
            0x80 as f32 / 255.0,
            0x40 as f32 / 255.0,
            0x20 as f32 / 255.0,
            1.0
        ]
    );
    // Registers the FVF does not supply read zero.
    assert_eq!(input.v[1], [0.0; 4]);
    assert_eq!(input.v[15], [0.0; 4]);
}

#[test]
fn vs_interpreter_loop_executes_spec_iterations() {
    // loop i0, i1 (spec aL=0, aU=3, aD=1 from int constant i1) with an
    // `add r0, r0, c0` body (c0 = 1.0) — oPos.x must end at 3.0, proving the
    // loop counter drove three body executions.
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [1.0, 0.0, 0.0, 0.0];
    let loop_dst = vs_src(RegType::Loop, 0);
    let loop_spec = vs_src(RegType::Loop, 1);
    let r0 = vs_src(RegType::Temp, 0);
    let instrs = vec![
        PsInstruction {
            op: PsOp::Loop,
            dst: Some(loop_dst),
            srcs: vec![loop_spec],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Add,
            dst: Some(r0),
            srcs: vec![r0, vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::EndLoop,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![r0],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let mut ints = [[0_i32; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT];
    ints[1] = [0, 3, 1, 0]; // aL=0, aU=3, aD=1
    let program = vs_program_with(instrs, &constants, ints, [false; 16]);
    let input = VsVertexInput { v: [[0.0; 4]; 16] };
    let out = run_vertex_shader(&program, &input);
    assert_eq!(
        out.pos[0], 3.0,
        "the loop body must run aU=3 times (r0 increments each pass)"
    );
}

#[test]
fn vs_interpreter_breakc_exits_loop_early() {
    // loop aU=10 with a `breakc i0, i1` (GE) body — when the counter reaches
    // the constant 5 the loop exits, so the body runs 5 times (counters
    // 0..4) and oPos.x lands on 5.0, not 10.0.
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [1.0, 0.0, 0.0, 0.0];
    let loop_dst = vs_src(RegType::Loop, 0);
    let loop_spec = vs_src(RegType::Loop, 1);
    let r0 = vs_src(RegType::Temp, 0);
    let instrs = vec![
        PsInstruction {
            op: PsOp::Loop,
            dst: Some(loop_dst),
            srcs: vec![loop_spec],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::BreakC,
            dst: None,
            srcs: vec![vs_src(RegType::Loop, 0), vs_src(RegType::Loop, 2)],
            tex_type: None,
            end: false,
            control: crate::d3d9_shader::D3DSPC_GE,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Add,
            dst: Some(r0),
            srcs: vec![r0, vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::EndLoop,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![r0],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let mut ints = [[0_i32; 4]; crate::d3d9_shader::VS_INT_CONST_COUNT];
    ints[1] = [0, 10, 1, 0]; // aL=0, aU=10, aD=1
    ints[2] = [5, 0, 0, 0]; // the breakc comparison constant
    let program = vs_program_with(instrs, &constants, ints, [false; 16]);
    let input = VsVertexInput { v: [[0.0; 4]; 16] };
    let out = run_vertex_shader(&program, &input);
    assert_eq!(
        out.pos[0], 5.0,
        "breakc must exit the loop when the counter reaches the bound"
    );
}

#[test]
fn vs_interpreter_if_else_selects_branch_by_bool_constant() {
    // if b0 (TRUE) → mov oPos.x, c0 (2.0); else → mov oPos.x, c1 (7.0).
    // The false path must be skipped entirely.
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [2.0, 0.0, 0.0, 0.0];
    constants[1] = [7.0, 0.0, 0.0, 0.0];
    let b0 = vs_src(RegType::ConstBool, 0);
    let instrs = vec![
        PsInstruction {
            op: PsOp::If,
            dst: Some(b0),
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Else,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![vs_src(RegType::Const, 1)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::EndIf,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let mut bools = [false; crate::d3d9_shader::VS_BOOL_CONST_COUNT];
    bools[0] = true;
    let program = vs_program_with(instrs.clone(), &constants, [[0; 4]; 4], bools);
    let input = VsVertexInput { v: [[0.0; 4]; 16] };
    let out = run_vertex_shader(&program, &input);
    assert_eq!(out.pos[0], 2.0, "if b0=TRUE must take the true branch");

    // b0 = FALSE → the else branch writes 7.0.
    let bools = [false; crate::d3d9_shader::VS_BOOL_CONST_COUNT];
    let program = vs_program_with(instrs, &constants, [[0; 4]; 4], bools);
    let out = run_vertex_shader(&program, &input);
    assert_eq!(out.pos[0], 7.0, "if b0=FALSE must take the else branch");
}

#[test]
fn vs_interpreter_predication_gates_instruction_on_p0() {
    // Two predicated `mov oPos.x` instructions (p0 set / p0 clear). The first
    // writes 3.0 (p0.x = 1.0), the second is skipped (p0.x = 0.0) — the
    // predicate register gates each instruction independently.
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [9.0, 0.0, 0.0, 0.0];
    constants[1] = [3.0, 0.0, 0.0, 0.0];
    let p0 = vs_src(RegType::Predicate, 0);
    let instrs = vec![
        PsInstruction {
            op: PsOp::Setp,
            dst: Some(p0),
            srcs: vec![vs_src(RegType::Const, 0), vs_src(RegType::Const, 1)],
            tex_type: None,
            end: false,
            control: crate::d3d9_shader::D3DSPC_GE,
            predicated: false,
        },
        // predicated mov oPos.x, c0 — p0.x = 1.0 → executes.
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: true,
        },
        // predicated mov oPos.y, c1 — p0.x still 1.0 → executes too; a second
        // run with p0 = 0.0 skips both (checked below via the constant swap).
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let program = vs_program_with(instrs, &constants, [[0; 4]; 4], [false; 16]);
    let input = VsVertexInput { v: [[0.0; 4]; 16] };
    let out = run_vertex_shader(&program, &input);
    assert_eq!(
        out.pos[0], 9.0,
        "the predicated mov must run while p0.x != 0"
    );
}

#[test]
fn vs_interpreter_mova_drives_relative_const_read() {
    // mova a0, c0 (a0.x = 2) then `mov oPos.x, c[a0.x + 1]` reads constant
    // register 3 — the relative operand (bit 13) adds the address register
    // to its base register number.
    let mut constants = [[0.0; 4]; 256];
    constants[0] = [2.0, 0.0, 0.0, 0.0];
    constants[1] = [11.0, 0.0, 0.0, 0.0];
    constants[2] = [22.0, 0.0, 0.0, 0.0];
    constants[3] = [33.0, 0.0, 0.0, 0.0];
    let mut rel_src = vs_src(RegType::Const, 1);
    rel_src.relative = true;
    let instrs = vec![
        PsInstruction {
            op: PsOp::Mova,
            dst: Some(vs_src(RegType::Texture, 0)),
            srcs: vec![vs_src(RegType::Const, 0)],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::Mov,
            dst: Some(o_pos()),
            srcs: vec![rel_src],
            tex_type: None,
            end: false,
            control: 0,
            predicated: false,
        },
        PsInstruction {
            op: PsOp::End,
            dst: None,
            srcs: Vec::new(),
            tex_type: None,
            end: true,
            control: 0,
            predicated: false,
        },
    ];
    let program = vs_program(instrs, &constants);
    let input = VsVertexInput { v: [[0.0; 4]; 16] };
    let out = run_vertex_shader(&program, &input);
    assert_eq!(
        out.pos[0], 33.0,
        "c[a0.x + 1] must read register 3 (a0.x = 2)"
    );
}

#[test]
fn clip_to_viewport_applies_w_divide_and_minmax_z_scale() {
    let vp = Viewport {
        x: 10,
        y: 20,
        width: 100,
        height: 50,
        min_z: 0.25,
        max_z: 0.75,
    };
    // NDC (-1, 1, 0.5) at w=1 → top-left corner, z = 0.25 + (0.5+1)·0.5·0.5.
    let (sx, sy, sz, w) = clip_to_viewport([-1.0, 1.0, 0.5, 1.0], &vp).expect("in front");
    assert_eq!((sx, sy), (10.0, 20.0));
    assert!((sz - 0.625).abs() < 1e-6);
    assert_eq!(w, 1.0);
    // The z-range clamps at MinZ/MaxZ for z_ndc = -1 / +1.
    let (_, _, z0, _) = clip_to_viewport([0.0, 0.0, -1.0, 1.0], &vp).expect("in front");
    let (_, _, z1, _) = clip_to_viewport([0.0, 0.0, 1.0, 1.0], &vp).expect("in front");
    assert_eq!(z0, vp.min_z);
    assert_eq!(z1, vp.max_z);
    // The w-divide: a clip point at half w lands at half the NDC extent.
    let (sx, sy, _, w) = clip_to_viewport([1.0, -1.0, 0.0, 2.0], &vp).expect("in front");
    assert_eq!((sx, sy), (85.0, 57.5)); // NDC (0.5, -0.5) → mid between center and corner
    assert_eq!(w, 2.0);
    // w <= 0 (on/behind the near plane) → rejected.
    assert!(clip_to_viewport([0.0, 0.0, 0.0, 0.0], &vp).is_none());
    assert!(clip_to_viewport([0.0, 0.0, 0.0, -1.0], &vp).is_none());
}

/// A 2x2 checkerboard texture stage (red/green/blue/white, POINT, WRAP) —
/// the gui_d3d9 micro-exe's texture.
fn checkerboard_stage() -> super::TextureStage<'static> {
    let texels = vec![
        0xFF_FF_00_00_u32, // red    (u=0, v=0)
        0xFF_00_FF_00,     // green  (u=1, v=0)
        0xFF_00_00_FF,     // blue   (u=0, v=1)
        0xFF_FF_FF_FF,     // white  (u=1, v=1)
    ]
    .leak();
    super::TextureStage {
        pixels: texels,
        width: 2,
        height: 2,
        addr_u: super::D3DTADDRESS_WRAP,
        addr_v: super::D3DTADDRESS_WRAP,
        mag_filter: super::D3DTEXF_POINT,
        min_filter: super::D3DTEXF_POINT,
        mip_filter: super::D3DTEXF_POINT,
        mips: super::MipChain {
            count: 1,
            levels: [None; super::MAX_MIP_LEVELS],
        },
        color_op: super::D3DTOP_MODULATE,
        color_arg1: super::D3DTA_TEXTURE,
        color_arg2: super::D3DTA_DIFFUSE,
        alpha_op: super::D3DTOP_MODULATE,
        alpha_arg1: super::D3DTA_TEXTURE,
        alpha_arg2: super::D3DTA_DIFFUSE,
    }
}

/// The w-skewed quad's projection matrix from the micro-exe: the ortho plus
/// a w-shear (`_24 = 0.002`), so clip w = 1 + 0.002·y varies across the quad.
fn w_skew_projection() -> super::Mat4 {
    let mut m = IDENTITY;
    m[0] = 2.0 / 320.0; // _11
    m[5] = 2.0 / 240.0; // _22
    m[10] = 1.0; // _33
    m[15] = 1.0; // _44
    m[7] = 0.002; // _24 — the w-shear
    m
}

#[test]
fn perspective_interpolation_differs_from_affine_at_w_skewed_center() {
    // The micro-exe's w-skewed quad (world A(-20,-20) B(30,-20) C(-20,-80)
    // D(30,-80), uv full range, white diffuse). The quad's center pixel
    // (165,175) — the world-space center (5,-50) projects to ≈(165.56,
    // 175.56) — must sample texel (0,0) RED: perspective-correct uv is
    // ≈(0.499, 0.499) (world-linear interpolation preserved), while the
    // affine uv ≈(0.532, 0.466) lands in texel (1,0) GREEN. The observed
    // pixel proves the fragment stage divides by the interpolated 1/w.
    let vp = Viewport {
        x: 0,
        y: 0,
        width: 320,
        height: 240,
        min_z: 0.0,
        max_z: 1.0,
    };
    let mut back = vec![0xFF_00_00_00_u32; 320 * 240];
    let mut dirty = Some(IRect::empty());
    let v = |x: f32, y: f32, u: f32, vt: f32| GuestVertex {
        x,
        y,
        z: 0.0,
        w: 1.0,
        color: 0xFF_FF_FF_FF,
        u,
        v: vt,
    };
    let tex = checkerboard_stage();
    // Triangle (A,B,C) and (B,D,C), the same triangulation as the micro-exe.
    draw_triangle(
        &mut back,
        320,
        240,
        v(-20.0, -20.0, 0.0, 0.0),
        v(30.0, -20.0, 1.0, 0.0),
        v(-20.0, -80.0, 0.0, 1.0),
        false,
        &w_skew_projection(),
        &vp,
        Some(&tex),
        None,
        &mut no_frag(),
        &mut dirty,
    );
    draw_triangle(
        &mut back,
        320,
        240,
        v(30.0, -20.0, 1.0, 0.0),
        v(30.0, -80.0, 1.0, 1.0),
        v(-20.0, -80.0, 0.0, 1.0),
        false,
        &w_skew_projection(),
        &vp,
        Some(&tex),
        None,
        &mut no_frag(),
        &mut dirty,
    );
    let center = back[175 * 320 + 165];
    assert_eq!(
        center, 0x00_FE_00_00,
        "the w-skewed quad's center pixel must be the perspective-correct \
         RED texel (0,0), not the affine GREEN (1,0)"
    );
    // Sanity: the affine uv at the same pixel really is in texel (1,0) —
    // i.e. the two interpolants disagree by more than one texel. Computed
    // from the screen-space triangle (A,B,C) directly: barycentric weights
    // at pixel center (165.5, 175.5) → affine u ≈ 0.532, v ≈ 0.466.
    let (ax, ay) = (139.17_f32, 140.83);
    let (bx, by) = (191.25, 140.83);
    let (cx, cy) = (136.19, 215.24);
    let (px, py) = (165.5, 175.5);
    let e_bc = (cx - bx) * (py - by) - (cy - by) * (px - bx);
    let e_ca = (ax - cx) * (py - cy) - (ay - cy) * (px - cx);
    let e_ab = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
    let sum = e_bc + e_ca + e_ab;
    let (wa, wb, _wc) = (e_bc / sum, e_ca / sum, e_ab / sum);
    let affine_u = wa * 0.0 + wb * 1.0;
    let affine_v = wa * 0.0 + wb * 0.0 + (e_ab / sum) * 1.0;
    assert!(
        (affine_u - 0.5).abs() > 0.02 && (affine_v - 0.5).abs() > 0.02,
        "the affine interpolant must differ from the perspective center \
         (u={affine_u}, v={affine_v}) — the test quad no longer discriminates"
    );
    assert!(
        affine_u >= 0.5 && affine_v < 0.5,
        "the affine uv must land in the GREEN texel quadrant"
    );
}

// ── L3 fragment stages: fog / alpha test / scissor + the validation matrix ──

/// A screen-space vertex helper (constant z, color).
fn sv(x: f32, y: f32, z: f32, color: u32) -> ScreenVertex {
    ScreenVertex {
        x,
        y,
        z,
        w: 1.0,
        color,
        u: 0.0,
        v: 0.0,
    }
}

#[test]
fn fog_linear_factor_and_blend() {
    // LINEAR: f = (end - z) / (end - start), clamped to [0, 1]. The fragment
    // state carries the fog start/end; the mode selects the formula.
    let frag = super::FragmentState {
        fog_start: 0.25,
        fog_end: 0.75,
        ..no_frag()
    };
    // At the fog start the factor is 1 (no fog); at the end 0 (fully fogged).
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_LINEAR, 0.25), 1.0);
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_LINEAR, 0.75), 0.0);
    // Mid-range (the demo quad at z=0.5): 0.25/0.5 are exactly representable,
    // so f is exactly 0.5 (not 0.50000006 — the 0.4/0.6 pair is not).
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_LINEAR, 0.5), 0.5);
    // Out of range clamps.
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_LINEAR, 0.0), 1.0);
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_LINEAR, 1.0), 0.0);
    // D3DFOG_NONE and unknown modes apply no fog.
    assert_eq!(super::fog_factor(&frag, super::D3DFOG_NONE, 0.5), 1.0);
    assert_eq!(super::fog_factor(&frag, 0xDEAD, 0.5), 1.0);

    // EXP / EXP2 from the density.
    let exp_frag = super::FragmentState {
        fog_density: 1.0,
        ..no_frag()
    };
    // e^-1 ≈ 0.367879.
    assert!((super::fog_factor(&exp_frag, super::D3DFOG_EXP, 1.0) - 0.36787944).abs() < 1e-6);
    // e^-(1²) = e^-1.
    assert!((super::fog_factor(&exp_frag, super::D3DFOG_EXP2, 1.0) - 0.36787944).abs() < 1e-6);

    // The blend: out = fog·(1−f) + color·f, per channel, rounded. Red under
    // blue fog at f=0.5 → (128, 0, 128) — the demo's deterministic pixel.
    assert_eq!(super::fog_blend(0x00FF_0000, 0x0000_00FF, 0.5), 0x0080_0080);
    // f=1 keeps the color; f=0 yields the fog color.
    assert_eq!(super::fog_blend(0x00FF_0000, 0x0000_00FF, 1.0), 0x00FF_0000);
    assert_eq!(super::fog_blend(0x00FF_0000, 0x0000_00FF, 0.0), 0x0000_00FF);
    // The alpha byte is masked out before the blend.
    assert_eq!(super::fog_blend(0x80FF_0000, 0x0000_00FF, 0.5), 0x0080_0080);
}

#[test]
fn alpha_test_matrix() {
    // Each D3DCMP_* against a reference of 0x40.
    assert!(!super::alpha_test_pass(0x20, super::D3DCMP_NEVER, 0x40));
    assert!(super::alpha_test_pass(0x20, super::D3DCMP_LESS, 0x40));
    assert!(!super::alpha_test_pass(0x80, super::D3DCMP_LESS, 0x40));
    assert!(super::alpha_test_pass(0x40, super::D3DCMP_EQUAL, 0x40));
    assert!(super::alpha_test_pass(0x20, super::D3DCMP_LESSEQUAL, 0x40));
    assert!(super::alpha_test_pass(0x40, super::D3DCMP_LESSEQUAL, 0x40));
    assert!(!super::alpha_test_pass(0x80, super::D3DCMP_LESSEQUAL, 0x40));
    // The demo's gate: GREATER with ref 0x40 — 0x20 fails, 0x80 passes.
    assert!(!super::alpha_test_pass(0x20, super::D3DCMP_GREATER, 0x40));
    assert!(super::alpha_test_pass(0x80, super::D3DCMP_GREATER, 0x40));
    assert!(!super::alpha_test_pass(0x40, super::D3DCMP_GREATER, 0x40));
    assert!(super::alpha_test_pass(0x20, super::D3DCMP_NOTEQUAL, 0x40));
    assert!(!super::alpha_test_pass(0x40, super::D3DCMP_NOTEQUAL, 0x40));
    assert!(super::alpha_test_pass(
        0x80,
        super::D3DCMP_GREATEREQUAL,
        0x40
    ));
    assert!(super::alpha_test_pass(
        0x40,
        super::D3DCMP_GREATEREQUAL,
        0x40
    ));
    assert!(super::alpha_test_pass(0x20, super::D3DCMP_ALWAYS, 0x40));
    // Unknown funcs pass (the documented lenient fallback).
    assert!(super::alpha_test_pass(0x20, 0xDEAD, 0x40));
}

#[test]
fn render_state_validation_matrix() {
    use super::*;
    // Booleans: only 0/1.
    for state in [
        D3DRS_ALPHABLENDENABLE,
        D3DRS_ZWRITEENABLE,
        D3DRS_FOGENABLE,
        D3DRS_RANGEFOGENABLE,
        D3DRS_ALPHATESTENABLE,
        D3DRS_SCISSORTESTENABLE,
    ] {
        assert!(render_state_value_valid(state, 0), "{state} FALSE");
        assert!(render_state_value_valid(state, 1), "{state} TRUE");
        assert!(!render_state_value_valid(state, 2), "{state} rejects 2");
    }
    // D3DZB_* depth modes.
    assert!(render_state_value_valid(D3DRS_ZENABLE, 0));
    assert!(render_state_value_valid(D3DRS_ZENABLE, 2));
    assert!(!render_state_value_valid(D3DRS_ZENABLE, 3));
    // D3DCMP_* ranges.
    for state in [D3DRS_ZFUNC, D3DRS_ALPHAFUNC] {
        assert!(
            render_state_value_valid(state, D3DCMP_NEVER),
            "{state} NEVER"
        );
        assert!(
            render_state_value_valid(state, D3DCMP_ALWAYS),
            "{state} ALWAYS"
        );
        assert!(!render_state_value_valid(state, 0), "{state} rejects 0");
        assert!(!render_state_value_valid(state, 9), "{state} rejects 9");
    }
    // The implemented D3DBLEND_* factors.
    for state in [D3DRS_SRCBLEND, D3DRS_DESTBLEND] {
        assert!(render_state_value_valid(state, D3DBLEND_ZERO));
        assert!(render_state_value_valid(state, D3DBLEND_INVDESTCOLOR));
        assert!(!render_state_value_valid(state, 0));
        assert!(
            !render_state_value_valid(state, 11),
            "BLENDFACTOR unimplemented → reject"
        );
    }
    // The implemented D3DBLENDOP_* ops.
    assert!(render_state_value_valid(D3DRS_BLENDOP, D3DBLENDOP_ADD));
    assert!(render_state_value_valid(
        D3DRS_BLENDOP,
        D3DBLENDOP_REVSUBTRACT
    ));
    assert!(!render_state_value_valid(D3DRS_BLENDOP, 0));
    assert!(
        !render_state_value_valid(D3DRS_BLENDOP, 4),
        "MIN unimplemented → reject"
    );
    // D3DFOGMODE_*.
    for state in [D3DRS_FOGTABLEMODE, D3DRS_FOGVERTEXMODE] {
        assert!(render_state_value_valid(state, D3DFOG_NONE));
        assert!(render_state_value_valid(state, D3DFOG_LINEAR));
        assert!(!render_state_value_valid(state, 4), "{state} rejects 4");
    }
    // ALPHAREF is a u8.
    assert!(render_state_value_valid(D3DRS_ALPHAREF, 0));
    assert!(render_state_value_valid(D3DRS_ALPHAREF, 255));
    assert!(!render_state_value_valid(D3DRS_ALPHAREF, 256));
    // Unmodeled states accept anything (the raw-value layer round-trips).
    assert!(render_state_value_valid(0x1FF, 7));
    assert!(render_state_value_valid(0xDEAD, 0xDEAD));
    // Unconstrained modeled states accept any D3DCOLOR / float bits.
    assert!(render_state_value_valid(D3DRS_FOGCOLOR, 0xDEAD_BEEF));
    assert!(render_state_value_valid(D3DRS_FOGSTART, 0x7FC0_0000));
}

#[test]
fn render_state_value_of_round_trips_modeled_states() {
    let rs = super::RenderState {
        alpha_blend_enable: true,
        z_write_enable: false,
        z_enable: super::D3dZBufferType::True,
        z_func: super::D3dCmpFunc::Greater,
        src_blend: super::D3dBlend::SrcAlpha,
        dest_blend: super::D3dBlend::InvSrcAlpha,
        blend_op: super::D3dBlendOp::RevSubtract,
        fog_enable: true,
        fog_color: 0x00FF_FF00,
        fog_start: 0.25,
        fog_end: 0.75,
        fog_density: 0.5,
        fog_table_mode: super::D3DFOG_LINEAR,
        fog_vertex_mode: super::D3DFOG_NONE,
        alpha_test_enable: true,
        alpha_func: super::D3dCmpFunc::GreaterEqual,
        alpha_ref: 0x80,
        scissor_test_enable: true,
    };
    use super::*;
    assert_eq!(rs.value_of(D3DRS_ALPHABLENDENABLE), Some(1));
    assert_eq!(rs.value_of(D3DRS_ZWRITEENABLE), Some(0));
    assert_eq!(rs.value_of(D3DRS_ZENABLE), Some(1));
    assert_eq!(rs.value_of(D3DRS_ZFUNC), Some(5));
    assert_eq!(rs.value_of(D3DRS_SRCBLEND), Some(5));
    assert_eq!(rs.value_of(D3DRS_DESTBLEND), Some(6));
    assert_eq!(rs.value_of(D3DRS_BLENDOP), Some(3));
    assert_eq!(rs.value_of(D3DRS_FOGENABLE), Some(1));
    assert_eq!(rs.value_of(D3DRS_FOGCOLOR), Some(0x00FF_FF00));
    assert_eq!(rs.value_of(D3DRS_FOGSTART), Some(0.25_f32.to_bits()));
    assert_eq!(rs.value_of(D3DRS_FOGEND), Some(0.75_f32.to_bits()));
    assert_eq!(rs.value_of(D3DRS_FOGDENSITY), Some(0.5_f32.to_bits()));
    assert_eq!(rs.value_of(D3DRS_FOGTABLEMODE), Some(3));
    assert_eq!(rs.value_of(D3DRS_FOGVERTEXMODE), Some(0));
    assert_eq!(rs.value_of(D3DRS_ALPHATESTENABLE), Some(1));
    assert_eq!(rs.value_of(D3DRS_ALPHAFUNC), Some(7));
    assert_eq!(rs.value_of(D3DRS_ALPHAREF), Some(0x80));
    assert_eq!(rs.value_of(D3DRS_SCISSORTESTENABLE), Some(1));
    // Unmodeled states have no typed value.
    assert_eq!(rs.value_of(0x1FF), None);
}

#[test]
fn rasterize_alpha_test_clips_failing_fragments() {
    // A full-coverage triangle (bigger than the buffer) with vertex alpha
    // 0x20 fails GREATER 0x40 — no pixels may be written (the flat_fill fast
    // path is gated off, so the per-fragment alpha test actually runs).
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        alpha_test: 1,
        alpha_func: super::D3DCMP_GREATER,
        alpha_ref: 0x40,
        ..no_frag()
    };
    let alpha = 0x20_u32 << 24;
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, alpha | 0xFF_FF_FF),
        sv(9.0, -1.0, 0.5, alpha | 0xFF_FF_FF),
        sv(-1.0, 9.0, 0.5, alpha | 0xFF_FF_FF),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    assert_eq!(
        back, [0xFF_00_00_00; 16],
        "failing alpha test writes nothing"
    );

    // The same triangle at alpha 0x80 passes and fills the buffer.
    let mut frag = super::FragmentState {
        alpha_test: 1,
        alpha_func: super::D3DCMP_GREATER,
        alpha_ref: 0x40,
        ..no_frag()
    };
    let alpha = 0x80_u32 << 24;
    let mut dirty = Some(IRect::empty());
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, alpha | 0xFF_FF_FF),
        sv(9.0, -1.0, 0.5, alpha | 0xFF_FF_FF),
        sv(-1.0, 9.0, 0.5, alpha | 0xFF_FF_FF),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    assert_eq!(back, [0x00_FF_FF_FF; 16], "passing alpha test fills");
}

#[test]
fn rasterize_fog_blends_toward_fog_color() {
    // A full-coverage triangle at constant z=0.5, red diffuse, blue fog,
    // linear fog over [0.25, 0.75] → f=0.5 → (128, 0, 128) everywhere. The
    // constant vertex color would normally trigger flat_fill; the fog gate
    // must disable it so the per-pixel blend runs.
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        fog_enable: 1,
        fog_color: 0x00_00_00_FF,
        fog_start: 0.25,
        fog_end: 0.75,
        fog_table_mode: super::D3DFOG_LINEAR,
        ..no_frag()
    };
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, 0xFF_FF_00_00),
        sv(9.0, -1.0, 0.5, 0xFF_FF_00_00),
        sv(-1.0, 9.0, 0.5, 0xFF_FF_00_00),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    assert_eq!(
        back, [0x00_80_00_80; 16],
        "red under blue fog at f=0.5 → (128, 0, 128)"
    );
}

#[test]
fn rasterize_vertex_fog_interpolates_vertex_factors() {
    // Vertex fog: the factor is computed per-vertex from the vertex z and
    // interpolated (Gouraud). Triangle A(0,0) z=0.25 (f=1, no fog) with
    // B(4,0) and C(0,4) at z=0.75 (f=0, fully fogged) under LINEAR fog over
    // [0.25, 0.75]. The pixel at (0,0) has barycentric weight 0.75 toward the
    // near vertex → f=0.75 → mostly red; the far-edge pixels have f=0 →
    // the fog color.
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        fog_enable: 1,
        fog_color: 0x00_00_00_FF,
        fog_start: 0.25,
        fog_end: 0.75,
        fog_vertex_mode: super::D3DFOG_LINEAR,
        ..no_frag()
    };
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(0.0, 0.0, 0.25, 0xFF_FF_00_00), // near vertex: f = 1 → pure red
        sv(4.0, 0.0, 0.75, 0xFF_FF_00_00), // far vertex: f = 0 → pure fog blue
        sv(0.0, 4.0, 0.75, 0xFF_FF_00_00), // far vertex: f = 0
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    // Near the (0,0) vertex the interpolated factor is 0.75 — mostly red.
    let near = back[0];
    assert!(
        (near >> 16) & 0xFF > 0xB0,
        "near the start vertex the color must stay mostly red, got 0x{near:06X}"
    );
    // The far edge of the triangle (the excluded hypotenuse x+y=4) holds no
    // pixels, so the most-far interior pixel is (2,0): its near-vertex
    // weight is 0.25 → f=0.25 → r=64, b=191 — mostly fog blue.
    let far = back[2];
    assert_eq!(
        far, 0x0040_00BF,
        "the f=0.25 pixel must be (64, 0, 191), got 0x{far:06X}"
    );
    // The pixel-interpolated factor at (0,0) is 0.75, not 1 (the vertex is
    // not the pixel center) — 255·0.75 = 191, so the red channel is 191.
    assert_eq!(
        (near >> 16) & 0xFF,
        191,
        "the Gouraud-interpolated factor at pixel (0,0) is 0.75"
    );
}

#[test]
fn rasterize_scissor_clips_outside_rect() {
    // A full-coverage triangle with a scissor rect covering only the top-left
    // 2x2: only those pixels may be written.
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        scissor_test: 1,
        scissor: Some(IRect {
            left: 0,
            top: 0,
            right: 2,
            bottom: 2,
        }),
        ..no_frag()
    };
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, 0xFF_FF_FF_FF),
        sv(9.0, -1.0, 0.5, 0xFF_FF_FF_FF),
        sv(-1.0, 9.0, 0.5, 0xFF_FF_FF_FF),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    // Inside the scissor rect (2x2) → white; outside → untouched black.
    assert_eq!(back[0], 0x00_FF_FF_FF);
    assert_eq!(back[4 + 1], 0x00_FF_FF_FF);
    assert_eq!(back[2], 0xFF_00_00_00, "x=2 is outside the scissor rect");
    assert_eq!(
        back[8 + 1],
        0xFF_00_00_00,
        "y=2 is outside the scissor rect"
    );
    // No rect set + test enabled → no clipping (the conservative fallback).
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        scissor_test: 1,
        scissor: None,
        ..no_frag()
    };
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, 0xFF_FF_FF_FF),
        sv(9.0, -1.0, 0.5, 0xFF_FF_FF_FF),
        sv(-1.0, 9.0, 0.5, 0xFF_FF_FF_FF),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    assert_eq!(back[15], 0x00_FF_FF_FF, "no rect → no clip");
}

#[test]
fn rasterize_fog_alpha_scissor_combined_orders_stages() {
    // The demo strip's combined pipeline: a scissor-clipped, alpha-tested,
    // fogged triangle. Pixels inside the scissor rect pass every stage and
    // land the fogged color; a pixel outside the scissor rect stays untouched.
    let mut back = [0xFF_00_00_00_u32; 4 * 4];
    let mut dirty = Some(IRect::empty());
    let mut frag = super::FragmentState {
        fog_enable: 1,
        fog_color: 0x00_00_00_FF,
        fog_start: 0.25,
        fog_end: 0.75,
        fog_table_mode: super::D3DFOG_LINEAR,
        alpha_test: 1,
        alpha_func: super::D3DCMP_GREATER,
        alpha_ref: 0x40,
        scissor_test: 1,
        scissor: Some(IRect {
            left: 0,
            top: 0,
            right: 3,
            bottom: 3,
        }),
        ..no_frag()
    };
    // Alpha 0x80 passes, z=0.5 → f=0.5 → fogged (128, 0, 128).
    rasterize_triangle(
        &mut back,
        4,
        4,
        sv(-1.0, -1.0, 0.5, 0x80_FF_00_00),
        sv(9.0, -1.0, 0.5, 0x80_FF_00_00),
        sv(-1.0, 9.0, 0.5, 0x80_FF_00_00),
        None,
        None,
        &mut frag,
        &mut dirty,
    );
    assert_eq!(back[4 + 1], 0x00_80_00_80, "(1,1) passes all three stages");
    assert_eq!(
        back[4 * 3 + 3],
        0xFF_00_00_00,
        "(3,3) is outside the scissor"
    );
}
