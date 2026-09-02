//! D3D9 P3 software-render handler tests: caps, clear, scene flags, transform / viewport state, triangle rasterization, and Present frame publishing.
use super::*;

/// `D3DFMT_A8R8G8B8` — the render-target format the L6 handlers accept.
const D3DFMT_A8R8G8B8: u32 = 21;

// ── P3 D3D9 software-render handlers ────────────────────────────────

#[test]
fn test_d3d9_caps_declare_pixel_shader_pipeline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let caps_va = 0x5000_u64;
    // GetDeviceCaps(this, adapter=0, type=HAL, pCaps).
    write_regs(&mut engine, 1, 0, 1, caps_va, 0);
    assert_return_value!(
        d3d9::handle_get_device_caps(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0 // D3D_OK
    );
    let mut read_u32_at = |offset: u64| -> u32 {
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(caps_va + offset, &mut bytes)
            .expect("read caps field");
        u32::from_le_bytes(bytes)
    };
    // Caps honesty: the ps_2_0 interpreter AND the vs_2_0 vertex stage are
    // implemented, so the caps report D3DPS_VERSION(2,0) and D3DVS_VERSION
    // (2,0) (0xFFFE0200 — the vs version tag is 0xFFFE0000, not the ps tag's
    // 0xFFFF0000), PixelShader1xMaxValue 1.0, and the vs_2_0 constant file.
    assert_eq!(
        read_u32_at(196),
        0xFFFE_0200,
        "VertexShaderVersion must be D3DVS_VERSION(2,0)"
    );
    assert_eq!(
        read_u32_at(200),
        256,
        "MaxVertexShaderConst must be 256 (vs_2_0 constant file)"
    );
    assert_eq!(
        read_u32_at(204),
        0xFFFF_0200,
        "PixelShaderVersion must be D3DPS_VERSION(2,0)"
    );
    assert_eq!(
        read_u32_at(208),
        1.0_f32.to_bits(),
        "PixelShader1xMaxValue must be 1.0"
    );
}

#[test]
fn test_d3d9_clear_fills_whole_backbuffer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
    }
    // Clear(this, Count=0, pRects=NULL, Flags=TARGET, Color=0xFFC80000).
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let back = &state.d3d9().d3d9_backbuffer;
    assert_eq!(back.len(), 12);
    for (index, pixel) in back.iter().enumerate() {
        assert_eq!(
            *pixel, 0x00_C8_00_00,
            "backbuffer pixel {index} must be the clear color (0RGB)"
        );
    }
    assert_eq!(
        state.d3d9().d3d9_dirty,
        None,
        "Clear marks the frame full-dirty"
    );
}

#[test]
fn test_d3d9_clear_fills_only_requested_rects() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
    }
    // One D3DRECT {1,1,3,3} at 0x6000.
    let rect_va = 0x6000_u64;
    for (i, v) in [1_i32, 1, 3, 3].iter().enumerate() {
        engine
            .mem_write(
                rect_va + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write rect field");
    }
    write_regs(&mut engine, 1, 1, rect_va, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_00_00_FF_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let back = &state.d3d9().d3d9_backbuffer;
    // Row y, column x → back[y * 4 + x].
    assert_eq!(
        back.first().copied(),
        Some(0),
        "outside rect stays unchanged"
    );
    assert_eq!(
        back.get(4 + 2).copied(),
        Some(0x00_00_00_FF),
        "inside rect filled"
    );
    assert_eq!(
        back.get(8 + 2).copied(),
        Some(0x00_00_00_FF),
        "inside rect filled"
    );
    assert_eq!(
        back.get(4).copied(),
        Some(0),
        "outside rect stays unchanged"
    );
}

#[test]
fn test_d3d9_begin_end_scene_flags() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_scene_active,
        crate::state::SceneState::Active
    );
    // A second BeginScene inside a scene fails.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_scene_active,
        crate::state::SceneState::Inactive
    );
    // EndScene outside a scene fails.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x8876_086c
    );
}

#[test]
fn test_d3d9_set_transform_and_viewport_state() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A known 4x4 matrix (column-major) at 0x6000.
    let matrix_va = 0x6000_u64;
    let mut matrix = [0.0_f32; 16];
    matrix[0] = 2.0;
    matrix[5] = 3.0;
    matrix[10] = 0.5;
    matrix[15] = 1.0;
    for (i, value) in matrix.iter().enumerate() {
        engine
            .mem_write(
                matrix_va + u64::try_from(i).unwrap_or(0) * 4,
                &value.to_le_bytes(),
            )
            .expect("write matrix element");
    }
    write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_world_matrix.map(f32::to_bits),
        matrix.map(f32::to_bits),
        "SetTransform must store the matrix unchanged"
    );

    // SetViewport: D3DVIEWPORT9 {X,Y,Width,Height,MinZ,MaxZ} at 0x6400.
    let vp_va = 0x6400_u64;
    for (i, v) in [4_u32, 5, 100, 50].iter().enumerate() {
        engine
            .mem_write(vp_va + u64::try_from(i).unwrap_or(0) * 4, &v.to_le_bytes())
            .expect("write viewport field");
    }
    engine
        .mem_write(vp_va + 16, &0.25_f32.to_le_bytes())
        .expect("write MinZ");
    engine
        .mem_write(vp_va + 20, &0.75_f32.to_le_bytes())
        .expect("write MaxZ");
    write_regs(&mut engine, 1, vp_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_viewport(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let (vp_x, vp_y, vp_w, vp_h, vp_min_z, vp_max_z) = state.d3d9().d3d9_viewport;
    assert_eq!((vp_x, vp_y, vp_w, vp_h), (4, 5, 100, 50));
    // Bit-exact float compare (0.25/0.75 are exactly representable).
    assert_eq!(vp_min_z.to_bits(), 0.25_f32.to_bits());
    assert_eq!(vp_max_z.to_bits(), 0.75_f32.to_bits());

    // GetViewport writes it back.
    let out_va = 0x6800_u64;
    write_regs(&mut engine, 1, out_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_viewport(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut out = [0_u8; 24];
    engine
        .mem_read(out_va, &mut out)
        .expect("read viewport out");
    assert_eq!(u32::from_le_bytes(out[0..4].try_into().expect("x")), 4);
    assert_eq!(u32::from_le_bytes(out[8..12].try_into().expect("w")), 100);
}

#[test]
fn test_d3d9_draw_primitive_up_rasterizes_triangle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 16;
        d3d.d3d9_backbuffer_height = 16;
        d3d.d3d9_backbuffer = vec![0_u32; 16 * 16];
        d3d.d3d9_viewport = (0, 0, 16, 16, 0.0, 1.0);
    }
    // FVF = XYZ | DIFFUSE.
    write_regs(&mut engine, 1, u64::from(0x0002_u32 | 0x0040_u32), 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_fvf(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    // Clear to red first.
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    // Full-frame triangle (NDC corners) with per-vertex colors.
    let data_va = 0x6000_u64;
    let vertices: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, 1.0, 0.0]];
    let colors: [u32; 3] = [0xFF_FF_00_00, 0xFF_00_FF_00, 0xFF_00_00_FF];
    for (i, vertex) in vertices.iter().enumerate() {
        for (j, component) in vertex.iter().enumerate() {
            engine
                .mem_write(
                    data_va
                        + u64::try_from(i).unwrap_or(0) * 16
                        + u64::try_from(j).unwrap_or(0) * 4,
                    &component.to_le_bytes(),
                )
                .expect("write vertex position");
        }
        engine
            .mem_write(
                data_va + u64::try_from(i).unwrap_or(0) * 16 + 12,
                &colors.get(i).copied().unwrap_or(0).to_le_bytes(),
            )
            .expect("write vertex color");
    }
    // DrawPrimitiveUP(this, TRIANGLELIST, 1, data, stride=16).
    write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 1, data_va, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &16_u64.to_le_bytes())
        .expect("write stride");
    assert_return_value!(
        d3d9::handle_draw_primitive_up(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    // The triangle spans the bottom-left half (bounded by the diagonal
    // from (0,0) to (16,16)): inside pixels are colored, outside pixels
    // keep the clear red.
    let back = &state.d3d9().d3d9_backbuffer;
    let pixel = |x: u32, y: u32| {
        back.get(usize::try_from(y).unwrap_or(0) * 16 + usize::try_from(x).unwrap_or(0))
            .copied()
    };
    assert_eq!(
        pixel(14, 1),
        Some(0x00_C8_00_00),
        "pixels above the diagonal keep the clear color"
    );
    let red_channel = (pixel(1, 14).unwrap_or(0) >> 16) & 0xFF;
    let green_channel = (pixel(14, 14).unwrap_or(0) >> 8) & 0xFF;
    let blue_channel = pixel(1, 1).unwrap_or(0) & 0xFF;
    assert!(red_channel > 0xB0, "bottom-left corner red-dominant");
    assert!(green_channel > 0xB0, "bottom-right corner green-dominant");
    assert!(blue_channel > 0xB0, "top-left corner blue-dominant");
    // The dirty region covers the whole frame.
    assert_eq!(state.d3d9().d3d9_dirty, None, "draw keeps full-frame dirty");

    // EndScene then Present publishes the frame.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_d3d9_present_publishes_surface_frame() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = (0_u32..12).collect();
        d3d.d3d9_present_hwnd = crate::handles::Hwnd::from(0x7777);
        d3d.d3d9_dirty = None;
    }
    // Unknown hwnd → window_client_size falls back to WindowState.
    state.window_state().window_width = 4;
    state.window_state().window_height = 3;

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(0x7777))
        .expect("Present must publish a SurfaceFrame");
    assert_eq!((frame.width, frame.height), (4, 3));
    // The row pitch is 64-padded (ADR-0001); the logical 4×3 content sits at
    // the start of each row and the padding tail stays zero.
    assert_eq!(frame.stride, 64);
    for (i, px) in (0_u32..12).enumerate() {
        let y = i / 4;
        let x = i % 4;
        assert_eq!(
            frame.pixel(x.try_into().unwrap_or(0), y.try_into().unwrap_or(0)),
            Some(px),
            "logical pixel ({x}, {y}) round-trips through the padded stride"
        );
    }
}

// ── L6 render-target handlers ─────────────────────────────────────

/// CreateRenderTarget(4x4, A8R8G8B8) → a surface object with its own texels.
#[test]
fn test_d3d9_create_render_target_allocates_surface() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 4, 4, u64::from(D3DFMT_A8R8G8B8), 0);
    // MultiSample=0 (NONE) @0x28, MultiSampleQuality=0 @0x30, Lockable=1
    // @0x38, ppSurface @0x40.
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write MultiSample");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let rt_va = u64::from_le_bytes(surf_bytes);
    let record = state
        .d3d9()
        .d3d9_render_targets
        .get(&rt_va)
        .expect("render-target record exists");
    assert_eq!((record.width, record.height), (4, 4));
    assert_eq!(record.pixels.len(), 16);
    assert!(record.pixels.iter().all(|&p| p == 0), "RT starts zeroed");

    // A multisample request above NONE is the honest D3DERR_INVALIDCALL.
    let pp2 = 0x7100_u64;
    write_regs(&mut engine, 1, 4, 4, u64::from(D3DFMT_A8R8G8B8), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &4_u32.to_le_bytes())
        .expect("write MultiSample=4");
    engine
        .mem_write(STACK_TOP + 0x40, &pp2.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x8876_086c_u64
    );
    let mut cleared = [0_u8; 8];
    engine
        .mem_read(pp2, &mut cleared)
        .expect("read failed surface ptr");
    assert_eq!(u64::from_le_bytes(cleared), 0, "failed Create writes NULL");
}

/// SetRenderTarget(0, rt) binds; GetRenderTarget(0) round-trips; NULL
/// rebinds the backbuffer; an unknown surface is the honest INVALIDCALL.
#[test]
fn test_d3d9_set_get_render_target_binding() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 4, 4, u64::from(D3DFMT_A8R8G8B8), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write MultiSample");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let rt_va = u64::from_le_bytes(surf_bytes);

    // SetRenderTarget(0, rt).
    write_regs(&mut engine, 1, 0, rt_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(state.d3d9().d3d9_render_target, rt_va);

    // GetRenderTarget(0, out) returns it.
    let out_rt = 0x7200_u64;
    write_regs(&mut engine, 1, 0, out_rt, 0, 0);
    assert_return_value!(
        d3d9::handle_get_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_rt, &mut out_bytes)
        .expect("read returned RT");
    assert_eq!(u64::from_le_bytes(out_bytes), rt_va);

    // SetRenderTarget(0, NULL) rebinds the backbuffer (returns 0).
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(state.d3d9().d3d9_render_target, 0);
    write_regs(&mut engine, 1, 0, out_rt, 0, 0);
    assert_return_value!(
        d3d9::handle_get_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    engine
        .mem_read(out_rt, &mut out_bytes)
        .expect("read unbound RT");
    assert_eq!(u64::from_le_bytes(out_bytes), 0, "unbound RT returns 0");

    // An unknown surface is the honest D3DERR_INVALIDCALL.
    write_regs(&mut engine, 1, 0, 0x9999, 0, 0);
    assert_return_value!(
        d3d9::handle_set_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x8876_086c_u64
    );
}

/// Clear + DrawPrimitiveUP route into the bound RT's texels, not the
/// backbuffer; the backbuffer is untouched.
#[test]
fn test_d3d9_clear_and_draw_route_to_bound_render_target() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 16;
        d3d.d3d9_backbuffer_height = 16;
        d3d.d3d9_backbuffer = vec![0x00_AB_CD_EF_u32; 16 * 16]; // sentinel
        d3d.d3d9_viewport = (0, 0, 16, 16, 0.0, 1.0);
    }

    // Create an 8x8 render target.
    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 8, 8, u64::from(D3DFMT_A8R8G8B8), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write MultiSample");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let rt_va = u64::from_le_bytes(surf_bytes);

    // The viewport must match the bound RT (a real app sets viewport =
    // RT size after binding); a 16x16 viewport would map the triangle into a
    // 16-wide screen space and clip into the 8-wide RT.
    {
        let d3d = state.d3d9();
        d3d.d3d9_viewport = (0, 0, 8, 8, 0.0, 1.0);
    }

    // Bind it, clear it red, draw a full-frame triangle.
    write_regs(&mut engine, 1, 0, rt_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    // The RT is clear red; the backbuffer keeps its sentinel.
    assert!(
        state
            .d3d9()
            .d3d9_render_targets
            .get(&rt_va)
            .is_some_and(|record| record.pixels.iter().all(|&p| p == 0x00_C8_00_00)),
        "RT clear must fill the RT texels"
    );
    assert!(
        state
            .d3d9()
            .d3d9_backbuffer
            .iter()
            .all(|&p| p == 0x00_AB_CD_EF),
        "RT clear must not touch the backbuffer"
    );

    // FVF + BeginScene + a full-frame triangle into the RT.
    write_regs(&mut engine, 1, u64::from(0x0002_u32 | 0x0040_u32), 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_fvf(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let data_va = 0x6000_u64;
    let vertices: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, 1.0, 0.0]];
    let colors: [u32; 3] = [0xFF_FF_00_00, 0xFF_00_FF_00, 0xFF_00_00_FF];
    for (i, vertex) in vertices.iter().enumerate() {
        for (j, component) in vertex.iter().enumerate() {
            engine
                .mem_write(
                    data_va
                        + u64::try_from(i).unwrap_or(0) * 16
                        + u64::try_from(j).unwrap_or(0) * 4,
                    &component.to_le_bytes(),
                )
                .expect("write vertex position");
        }
        engine
            .mem_write(
                data_va + u64::try_from(i).unwrap_or(0) * 16 + 12,
                &colors.get(i).copied().unwrap_or(0).to_le_bytes(),
            )
            .expect("write vertex color");
    }
    write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 1, data_va, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &16_u64.to_le_bytes())
        .expect("write stride");
    assert_return_value!(
        d3d9::handle_draw_primitive_up(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // The RT now holds the triangle's colors; the backbuffer is untouched.
    let rt_pixels = state
        .d3d9()
        .d3d9_render_targets
        .get(&rt_va)
        .expect("RT exists")
        .pixels
        .clone();
    let back_pixels = state.d3d9().d3d9_backbuffer.clone();
    let rt_pixel = |x: u32, y: u32| {
        rt_pixels
            .get(usize::try_from(y).unwrap_or(0) * 8 + usize::try_from(x).unwrap_or(0))
            .copied()
    };
    assert!(
        (rt_pixel(1, 7).unwrap_or(0) >> 16) & 0xFF > 0xB0,
        "bottom-left corner of the RT is red-dominant (triangle red vertex)"
    );
    assert!(
        (rt_pixel(7, 7).unwrap_or(0) >> 8) & 0xFF > 0xB0,
        "bottom-right corner of the RT is green-dominant (triangle green vertex)"
    );
    assert!(
        (rt_pixel(1, 1).unwrap_or(0) & 0xFF) > 0xB0,
        "top-left corner of the RT is blue-dominant (triangle blue vertex)"
    );
    assert!(
        back_pixels.iter().all(|&p| p == 0x00_AB_CD_EF),
        "the backbuffer must be untouched by RT-bound draws"
    );

    // The GetDesc of the RT surface reports its dims + format.
    let desc_va = 0x7400_u64;
    write_regs(&mut engine, rt_va, desc_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_get_desc(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut desc_bytes = [0_u8; 32];
    engine
        .mem_read(desc_va, &mut desc_bytes)
        .expect("read surface desc");
    let format = u32::from_le_bytes(desc_bytes[0..4].try_into().unwrap_or([0; 4]));
    let width = u32::from_le_bytes(desc_bytes[24..28].try_into().unwrap_or([0; 4]));
    let height = u32::from_le_bytes(desc_bytes[28..32].try_into().unwrap_or([0; 4]));
    assert_eq!(format, D3DFMT_A8R8G8B8, "RT GetDesc reports its format");
    assert_eq!((width, height), (8, 8), "RT GetDesc reports its dims");
}

/// Surface LockRect/UnlockRect round-trips an offscreen RT's texels.
#[test]
fn test_d3d9_render_target_lock_unlock_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 4, 4, u64::from(D3DFMT_A8R8G8B8), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write MultiSample");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let rt_va = u64::from_le_bytes(surf_bytes);

    // LockRect → pitch 16, pBits; write a texel; UnlockRect lands it.
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, rt_va, locked_rect, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut pitch_bytes = [0_u8; 4];
    engine
        .mem_read(locked_rect, &mut pitch_bytes)
        .expect("read pitch");
    assert_eq!(u32::from_le_bytes(pitch_bytes), 16, "4x4 pitch must be 16");
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    engine
        .mem_write(p_bits, &0xFF_12_34_56_u32.to_le_bytes())
        .expect("write texel");
    write_regs(&mut engine, rt_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        state
            .d3d9()
            .d3d9_render_targets
            .get(&rt_va)
            .expect("RT")
            .pixels[0],
        0xFF_12_34_56,
        "RT UnlockRect must copy the texel back"
    );

    // Release the RT: record gone, lock state cleared, binding reset.
    write_regs(&mut engine, rt_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    assert!(!state.d3d9().d3d9_render_targets.contains_key(&rt_va));
}

/// D3DFVF_XYZ | D3DFVF_DIFFUSE — position (12 bytes) + diffuse (4 bytes) = stride 16.
const FVF_XYZ_DIFFUSE: u32 = 0x0002 | 0x0040;
/// D3DPT_TRIANGLELIST.
const D3DPT_TRIANGLELIST: u32 = 4;

/// DrawPrimitive (buffer form) with a bound vertex buffer — triangle in bottom-left half.
#[test]
fn test_d3d9_draw_primitive_triangle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor before any Create* allocation.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // ── Minimal device state: backbuffer, viewport, scene ───────────────
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 16;
        d3d.d3d9_backbuffer_height = 16;
        d3d.d3d9_backbuffer = vec![0_u32; 16 * 16];
        d3d.d3d9_viewport = (0, 0, 16, 16, 0.0, 1.0);
    }

    write_regs(&mut engine, 1, u64::from(FVF_XYZ_DIFFUSE), 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_fvf(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // Clear to red.
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // ── CreateVertexBuffer — 3 vertices × stride 16 ───────────────────
    let pp_buffer = 0x7000_u64;
    write_regs(&mut engine, 1, 3 * 16, 0, u64::from(FVF_XYZ_DIFFUSE), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &1_u32.to_le_bytes()) // D3DPOOL_MANAGED
        .expect("write pool");
    engine
        .mem_write(STACK_TOP + 0x30, &pp_buffer.to_le_bytes())
        .expect("write ppBuffer");
    assert_return_value!(
        d3d9::handle_create_vertex_buffer(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut buf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_buffer, &mut buf_bytes)
        .expect("read buffer ptr");
    let vb_va = u64::from_le_bytes(buf_bytes);
    assert_ne!(vb_va, 0, "CreateVertexBuffer must return an object");

    // Lock entire buffer and fill three XYZRHW + diffuse vertices.
    let pp_data = 0x7100_u64;
    write_regs(&mut engine, vb_va, 0, 0, pp_data, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let mut data_bytes = [0_u8; 8];
    engine
        .mem_read(pp_data, &mut data_bytes)
        .expect("read lock ptr");
    let data_va = u64::from_le_bytes(data_bytes);

    // Three vertices forming a triangle covering the bottom-left half.
    // Each vertex: x, y, z (f32) + diffuse (u32 BGRA).
    let vertices: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, 1.0, 0.0]];
    let colors: [u32; 3] = [0xFF_FF_00_00, 0xFF_00_FF_00, 0xFF_00_00_FF];
    for (i, vertex) in vertices.iter().enumerate() {
        for (j, component) in vertex.iter().enumerate() {
            engine
                .mem_write(
                    data_va
                        + u64::try_from(i).unwrap_or(0) * 16
                        + u64::try_from(j).unwrap_or(0) * 4,
                    &component.to_le_bytes(),
                )
                .expect("write vertex pos");
        }
        engine
            .mem_write(
                data_va + u64::try_from(i).unwrap_or(0) * 16 + 12,
                &colors[i].to_le_bytes(),
            )
            .expect("write diffuse");
    }
    assert_return_value!(
        d3d9::handle_vertex_buffer_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // ── Bind the vertex buffer and draw ────────────────────────────────
    write_regs(&mut engine, 1, 0, vb_va, 0, 0); // stream 0, offset 0
    engine
        .mem_write(STACK_TOP + 0x28, &16_u32.to_le_bytes())
        .expect("write stride");
    assert_return_value!(
        d3d9::handle_set_stream_source(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // DrawPrimitive(TRIANGLELIST, startVertex=0, primitiveCount=1).
    write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 0, 1, 0);
    assert_return_value!(
        d3d9::handle_draw_primitive(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // Pixel checks mirror the DrawPrimitiveUP test.
    let back = &state.d3d9().d3d9_backbuffer;
    let pixel = |x: u32, y: u32| {
        back.get(usize::try_from(y).unwrap_or(0) * 16 + usize::try_from(x).unwrap_or(0))
            .copied()
    };
    assert_eq!(
        pixel(14, 1),
        Some(0x00_C8_00_00),
        "pixels above diagonal keep clear color"
    );
    let red_channel = (pixel(1, 14).unwrap_or(0) >> 16) & 0xFF;
    let green_channel = (pixel(14, 14).unwrap_or(0) >> 8) & 0xFF;
    let blue_channel = pixel(1, 1).unwrap_or(0) & 0xFF;
    assert!(
        red_channel > 0xB0,
        "bottom-left corner red-dominant ({red_channel:#x})"
    );
    assert!(
        green_channel > 0xB0,
        "bottom-right corner green-dominant ({green_channel:#x})"
    );
    assert!(
        blue_channel > 0xB0,
        "top-left corner blue-dominant ({blue_channel:#x})"
    );
    assert_eq!(state.d3d9().d3d9_dirty, None, "draw keeps full-frame dirty");

    // EndScene + Present (verifies the full pipeline wired together).
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

/// Clear sets the dirty flag; Present clears it after publishing.
#[test]
fn test_d3d9_clear_sets_dirty_and_present_clears() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 4;
        d3d.d3d9_backbuffer = vec![0_u32; 4 * 4];
        d3d.d3d9_viewport = (0, 0, 4, 4, 0.0, 1.0);
    }

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // Before clear: dirty flag is None.
    assert_eq!(
        state.d3d9().d3d9_dirty,
        None,
        "no dirty region before clear"
    );

    // Clear sets the dirty flag (None = full frame).
    write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0xFF_00_00_00_u32.to_le_bytes())
        .expect("write clear color");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(state.d3d9().d3d9_dirty, None, "Clear sets full-frame dirty");

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    // Present publishes and clears the dirty flag.
    assert_eq!(state.d3d9().d3d9_dirty, None, "Present clears dirty flag");
}
