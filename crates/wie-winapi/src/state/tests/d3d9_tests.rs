//! D3D9 P3 software-render handler tests: caps, clear, scene flags, transform / viewport state, triangle rasterization, and Present frame publishing.
use super::*;

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
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_end_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_begin_scene(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
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
            &mut state,
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
            &mut state,
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
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
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
            &mut state,
        )),
        0
    );
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(0x7777))
        .expect("Present must publish a SurfaceFrame");
    assert_eq!((frame.width, frame.height), (4, 3));
    assert_eq!(&frame.pixels[..], &(0_u32..12).collect::<Vec<u32>>()[..]);
}
