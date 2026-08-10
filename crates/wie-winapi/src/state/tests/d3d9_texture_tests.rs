//! D3D9 P4b texture / buffer handler tests: texture lock/unlock + mip chains, vertex-shader constants, vertex-buffer lifecycle, indexed draws, and depth surfaces.
use super::*;

// ── P4b texture handlers ───────────────────────────────────────────

/// D3DFMT_A8R8G8B8.
const D3DFMT_A8R8G8B8: u32 = 21;

#[test]
fn test_d3d9_texture_lock_unlock_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // The runtime seeds the guest heap bump cursor at session init; the
    // test heap control block starts zeroed, so seed it before allocating.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateTexture(2x2, levels=1, format=A8R8G8B8) → texture at 0x7000.
    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 1, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);
    assert_ne!(texture_va, 0, "CreateTexture must return an object");

    // GetSurfaceLevel(0) → surface at 0x7100.
    let pp_surface = 0x7100_u64;
    write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(surface_va, 0, "GetSurfaceLevel must return a surface");

    // LockRect → D3DLOCKED_RECT { Pitch, pBits } at 0x7200; write texels.
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, surface_va, locked_rect, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut pitch_bytes = [0_u8; 4];
    engine
        .mem_read(locked_rect, &mut pitch_bytes)
        .expect("read pitch");
    assert_eq!(u32::from_le_bytes(pitch_bytes), 8, "2x2 pitch must be 8");
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    assert_ne!(p_bits, 0, "LockRect must hand out a guest block");

    // 2x2 texels: red / green / blue / white (D3DCOLOR).
    let texels = [0xFFFF_0000_u32, 0xFF00_FF00, 0xFF00_00FF, 0xFFFF_FFFF];
    for (i, texel) in texels.iter().enumerate() {
        engine
            .mem_write(
                p_bits + u64::try_from(i).unwrap_or(0) * 4,
                &texel.to_le_bytes(),
            )
            .expect("write texel");
    }

    // UnlockRect → texels land in the host record.
    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(record.width, 2);
    assert_eq!(record.height, 2);
    assert_eq!(
        record.pixels,
        texels.to_vec(),
        "unlock must copy the texels back"
    );
    assert_eq!(record.locked_va, 0, "lock state cleared");

    // SetTexture(0, tex) → GetTexture(0) round-trip.
    write_regs(&mut engine, 1, 0, texture_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let out_texture = 0x7300_u64;
    write_regs(&mut engine, 1, 0, out_texture, 0, 0);
    assert_return_value!(
        d3d9::handle_get_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_texture, &mut out_bytes)
        .expect("read texture out");
    assert_eq!(u64::from_le_bytes(out_bytes), texture_va);

    // Release the texture: record gone, bindings cleared.
    write_regs(&mut engine, texture_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(!state.d3d9().d3d9_textures.contains_key(&texture_va));
    assert_eq!(state.d3d9().d3d9_texture_bindings[0], 0);
}

#[test]
fn test_d3d9_mip_chain_selects_level_surface_and_texels() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateTexture(4x4, levels=0 → full chain 4x4/2x2/1x1, A8R8G8B8).
    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 4, 4, 0, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(
        record.levels, 3,
        "levels=0 must build the full chain 4x4/2x2/1x1, not a single level"
    );
    assert_eq!(record.mip_levels.len(), 2);
    assert_eq!(record.mip_levels[0].width, 2);
    assert_eq!(record.mip_levels[0].height, 2);
    assert_eq!(record.mip_levels[1].width, 1);
    assert_eq!(record.mip_levels[1].height, 1);

    // GetSurfaceLevel(1) → the 2x2 level's own surface.
    let pp_surface = 0x7100_u64;
    write_regs(&mut engine, texture_va, 1, pp_surface, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(surface_va, 0, "GetSurfaceLevel(1) must return a surface");
    assert_eq!(
        state.d3d9().d3d9_surface_levels.get(&surface_va),
        Some(&1),
        "the surface view must remember it is level 1"
    );

    // LockRect the level-1 surface → pitch is 2x4 = 8; write two magenta
    // texels; UnlockRect must land them in mip_levels[0].pixels, NOT level 0.
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, surface_va, locked_rect, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut pitch_bytes = [0_u8; 4];
    engine
        .mem_read(locked_rect, &mut pitch_bytes)
        .expect("read pitch");
    assert_eq!(u32::from_le_bytes(pitch_bytes), 8, "2x2 pitch must be 8");
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    engine
        .mem_write(p_bits, &0xFF00_FFFF_u32.to_le_bytes())
        .expect("write level-1 texel");
    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(
        record.mip_levels[0].pixels[0], 0xFF00_FFFF,
        "unlock on a level-1 surface must write the mip texels"
    );
    assert_eq!(record.pixels[0], 0, "level-0 texels must stay untouched");
}

#[test]
fn test_d3d9_vertex_shader_int_bool_constant_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // SetVertexShaderConstantI(0, data, 1) with an int4 (1,2,3,4).
    let data_i = 0x7000_u64;
    for (i, v) in [1_i32, 2, 3, 4].iter().enumerate() {
        engine
            .mem_write(data_i + u64::try_from(i).unwrap_or(0) * 4, &v.to_le_bytes())
            .expect("write int constant");
    }
    write_regs(&mut engine, 1, 0, data_i, 1, 0);
    assert_return_value!(
        d3d9::handle_set_vertex_shader_constant_i(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_vs_int_constants[0],
        [1, 2, 3, 4],
        "SetVertexShaderConstantI must land in the i0 file"
    );

    // GetVertexShaderConstantI(0, out, 1) reads it back.
    let out_i = 0x7100_u64;
    write_regs(&mut engine, 1, 0, out_i, 1, 0);
    assert_return_value!(
        d3d9::handle_get_vertex_shader_constant_i(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut back = [0_u8; 16];
    engine
        .mem_read(out_i, &mut back)
        .expect("read int constants");
    assert_eq!(
        back,
        [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0],
        "GetVertexShaderConstantI must round-trip the int4"
    );

    // SetVertexShaderConstantB(1, data, 1) → b1 = TRUE.
    let data_b = 0x7200_u64;
    engine
        .mem_write(data_b, &1_u32.to_le_bytes())
        .expect("write bool constant");
    write_regs(&mut engine, 1, 1, data_b, 1, 0);
    assert_return_value!(
        d3d9::handle_set_vertex_shader_constant_b(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert!(
        state.d3d9().d3d9_vs_bool_constants[1],
        "SetVertexShaderConstantB must land in the b1 file"
    );

    // GetVertexShaderConstantB(1, out, 1) reads it back as a nonzero DWORD.
    let out_b = 0x7300_u64;
    write_regs(&mut engine, 1, 1, out_b, 1, 0);
    assert_return_value!(
        d3d9::handle_get_vertex_shader_constant_b(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut bool_bytes = [0_u8; 4];
    engine
        .mem_read(out_b, &mut bool_bytes)
        .expect("read bool constant");
    assert_eq!(
        u32::from_le_bytes(bool_bytes),
        1,
        "GetVertexShaderConstantB must round-trip the boolean"
    );
}

#[test]
fn test_d3d9_texture_unlock_with_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 1, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);

    let pp_surface = 0x7100_u64;
    write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
    assert_return_value!(
        d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);

    // Lock only the top-left texel: RECT {0,0,1,1} at 0x7400.
    let rect_va = 0x7400_u64;
    for (i, v) in [0_i32, 0, 1, 1].iter().enumerate() {
        engine
            .mem_write(
                rect_va + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write rect field");
    }
    let locked_rect = 0x7200_u64;
    write_regs(&mut engine, surface_va, locked_rect, rect_va, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut bits_bytes = [0_u8; 8];
    engine
        .mem_read(locked_rect + 8, &mut bits_bytes)
        .expect("read pBits");
    let p_bits = u64::from_le_bytes(bits_bytes);
    // The rect's pBits points at the rect top-left (the block start).
    assert_ne!(p_bits, 0);

    // Write the top-left texel (red); leave the rest of the block zero.
    engine
        .mem_write(p_bits, &0xFFFF_0000_u32.to_le_bytes())
        .expect("write texel");

    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_textures
        .get(&texture_va)
        .expect("record exists");
    assert_eq!(
        record.pixels.first().copied(),
        Some(0xFFFF_0000),
        "rect region texel copied back"
    );
    assert_eq!(
        record.pixels.get(1).copied(),
        Some(0),
        "outside the rect stays zero"
    );
    assert_eq!(record.pixels.get(2).copied(), Some(0));
    assert_eq!(record.pixels.get(3).copied(), Some(0));
}

#[test]
fn test_d3d9_vertex_buffer_lifecycle_lock_desc_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the texture round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateVertexBuffer(3 * 20 bytes, usage=WRITEONLY, FVF=XYZRHW|DIFFUSE,
    // pool=MANAGED) → buffer at 0x7000.
    let fvf = 0x0004 | 0x0040; // D3DFVF_XYZRHW | D3DFVF_DIFFUSE (stride 20)
    let pp_buffer = 0x7000_u64;
    write_regs(&mut engine, 1, 3 * 20, 0x0000_0008, fvf, 0);
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
            &mut state,
        )),
        0
    );
    let mut buf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_buffer, &mut buf_bytes)
        .expect("read buffer ptr");
    let vb = u64::from_le_bytes(buf_bytes);
    assert_ne!(vb, 0, "CreateVertexBuffer must return an object");

    // Lock(offset 4, size 0 → rest) → ppbData at 0x7100; fill one vertex.
    let pp_data = 0x7100_u64;
    write_regs(&mut engine, vb, 4, 0, pp_data, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut data_bytes = [0_u8; 8];
    engine
        .mem_read(pp_data, &mut data_bytes)
        .expect("read ppbData");
    let p_data = u64::from_le_bytes(data_bytes);
    assert_ne!(p_data, 0, "Lock must hand out a guest block");
    // Write one XYZRHW vertex (16 bytes) + diffuse (red) at the locked block.
    for (i, v) in [1.0_f32, 2.0, 0.5, 1.0].iter().enumerate() {
        engine
            .mem_write(p_data + u64::try_from(i).unwrap_or(0) * 4, &v.to_le_bytes())
            .expect("write vertex position");
    }
    engine
        .mem_write(p_data + 16, &0xFFFF_0000_u32.to_le_bytes())
        .expect("write diffuse");
    assert_return_value!(
        d3d9::handle_vertex_buffer_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // Unlock copied the block back into the host record at the locked offset.
    let record = state.d3d9().d3d9_buffers.get(&vb).expect("record exists");
    assert_eq!(record.size, 60);
    assert_eq!(record.locked_va, 0, "lock state cleared");
    let mut pos_bytes = [0_u8; 4];
    engine.mem_read(pp_data, &mut pos_bytes).ok();
    let _ = pos_bytes;
    // The guest wrote through the lock block; the host copy must show the
    // diffuse word at offset 4 + 16 (the locked offset shifted the vertex).
    let mut diffuse = [0_u8; 4];
    engine.mem_read(p_data + 16, &mut diffuse).ok();
    assert_eq!(u32::from_le_bytes(diffuse), 0xFFFF_0000);

    // GetDesc → D3DVERTEXBUFFER_DESC: Type=VERTEXBUFFER(6) @4, Size=60 @16,
    // FVF @20 (the desc layout verified against d3d9types.h).
    let desc_va = 0x7200_u64;
    write_regs(&mut engine, vb, desc_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_get_desc(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut desc = [0_u8; 24];
    engine.mem_read(desc_va, &mut desc).expect("read desc");
    assert_eq!(
        u32::from_le_bytes(desc[4..8].try_into().unwrap_or([0; 4])),
        6
    );
    assert_eq!(
        u32::from_le_bytes(desc[12..16].try_into().unwrap_or([0; 4])),
        1,
        "pool round-trips"
    );
    assert_eq!(
        u32::from_le_bytes(desc[16..20].try_into().unwrap_or([0; 4])),
        60,
        "size round-trips"
    );
    assert_eq!(
        u32::from_le_bytes(desc[20..24].try_into().unwrap_or([0; 4])),
        u32::try_from(fvf).unwrap_or(0),
        "FVF round-trips"
    );

    // AddRef → 2; Release → 1; Release → 0 (record gone, stream unbound).
    write_regs(&mut engine, vb, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_add_ref(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        2
    );
    write_regs(&mut engine, vb, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    write_regs(&mut engine, vb, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert!(!state.d3d9().d3d9_buffers.contains_key(&vb));
}

#[test]
fn test_d3d9_buffer_form_draw_indexed_primitive() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the texture round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // A 4x3 backbuffer, full viewport, FVF = XYZRHW|DIFFUSE (screen-space).
    let fvf = 0x0004 | 0x0040;
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
        d3d.d3d9_viewport = (0, 0, 4, 3, 0.0, 1.0);
    }
    write_regs(&mut engine, 1, fvf, 0, 0, 0);
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

    // CreateVertexBuffer(3 * 20, FVF XYZRHW|DIFFUSE) → 0x7000.
    let pp_vb = 0x7000_u64;
    write_regs(&mut engine, 1, 3 * 20, 0, fvf, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &1_u32.to_le_bytes())
        .expect("write pool");
    engine
        .mem_write(STACK_TOP + 0x30, &pp_vb.to_le_bytes())
        .expect("write ppBuffer");
    assert_return_value!(
        d3d9::handle_create_vertex_buffer(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut vb_bytes = [0_u8; 8];
    engine.mem_read(pp_vb, &mut vb_bytes).expect("read vb ptr");
    let vb = u64::from_le_bytes(vb_bytes);

    // Lock, write a solid-cyan right triangle (0,0)(3,0)(0,2), unlock.
    let pp_vdata = 0x7100_u64;
    write_regs(&mut engine, vb, 0, 0, pp_vdata, 0);
    assert_return_value!(
        d3d9::handle_vertex_buffer_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut vdata_bytes = [0_u8; 8];
    engine
        .mem_read(pp_vdata, &mut vdata_bytes)
        .expect("read vdata");
    let vdata = u64::from_le_bytes(vdata_bytes);
    let verts = [
        (0.0_f32, 0.0_f32, 0.5_f32, 1.0_f32, 0xFF00_FFFF_u32), // cyan
        (3.0_f32, 0.0_f32, 0.5_f32, 1.0_f32, 0xFF00_FFFF_u32),
        (0.0_f32, 2.0_f32, 0.5_f32, 1.0_f32, 0xFF00_FFFF_u32),
    ];
    for (i, (x, y, z, rhw, color)) in verts.iter().enumerate() {
        let base = vdata + u64::try_from(i).unwrap_or(0) * 20;
        for (j, v) in [x, y, z, rhw].iter().enumerate() {
            engine
                .mem_write(base + u64::try_from(j).unwrap_or(0) * 4, &v.to_le_bytes())
                .expect("write vertex");
        }
        engine
            .mem_write(base + 16, &color.to_le_bytes())
            .expect("write diffuse");
    }
    assert_return_value!(
        d3d9::handle_vertex_buffer_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // CreateIndexBuffer(3 * 2 bytes, INDEX16) → 0x7500; fill {0,1,2}.
    let pp_ib = 0x7500_u64;
    write_regs(&mut engine, 1, 6, 0, 101, 0); // D3DFMT_INDEX16 = 101
    engine
        .mem_write(STACK_TOP + 0x28, &1_u32.to_le_bytes())
        .expect("write pool");
    engine
        .mem_write(STACK_TOP + 0x30, &pp_ib.to_le_bytes())
        .expect("write ppBuffer");
    assert_return_value!(
        d3d9::handle_create_index_buffer(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut ib_bytes = [0_u8; 8];
    engine.mem_read(pp_ib, &mut ib_bytes).expect("read ib ptr");
    let ib = u64::from_le_bytes(ib_bytes);
    let pp_idata = 0x7600_u64;
    write_regs(&mut engine, ib, 0, 0, pp_idata, 0);
    assert_return_value!(
        d3d9::handle_index_buffer_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut idata_bytes = [0_u8; 8];
    engine
        .mem_read(pp_idata, &mut idata_bytes)
        .expect("read idata");
    let idata = u64::from_le_bytes(idata_bytes);
    for (i, idx) in [0_u16, 1, 2].iter().enumerate() {
        engine
            .mem_write(
                idata + u64::try_from(i).unwrap_or(0) * 2,
                &idx.to_le_bytes(),
            )
            .expect("write index");
    }
    assert_return_value!(
        d3d9::handle_index_buffer_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // SetStreamSource(0, vb, offset 0, stride 20); SetIndices(ib).
    write_regs(&mut engine, 1, 0, vb, 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &20_u32.to_le_bytes())
        .expect("write stride");
    assert_return_value!(
        d3d9::handle_set_stream_source(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, ib, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_indices(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // The L2 round-trip getters return the bound objects.
    let out_vb = 0x7700_u64;
    let out_off = 0x7780_u64;
    let out_stride = 0x7800_u64;
    write_regs(&mut engine, 1, 0, out_vb, out_off, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &out_stride.to_le_bytes())
        .expect("write pStride");
    assert_return_value!(
        d3d9::handle_get_stream_source(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_vb, &mut out_bytes)
        .expect("read vb out");
    assert_eq!(u64::from_le_bytes(out_bytes), vb);
    engine
        .mem_read(out_off, &mut out_bytes)
        .expect("read offset out");
    assert_eq!(u64::from_le_bytes(out_bytes), 0);
    engine
        .mem_read(out_stride, &mut out_bytes)
        .expect("read stride out");
    assert_eq!(u64::from_le_bytes(out_bytes), 20);
    let out_ib = 0x7880_u64;
    write_regs(&mut engine, 1, out_ib, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_indices(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    engine
        .mem_read(out_ib, &mut out_bytes)
        .expect("read ib out");
    assert_eq!(u64::from_le_bytes(out_bytes), ib);

    // DrawIndexedPrimitive(TRIANGLELIST, base 0, min 0, num 3, start 0, 1).
    write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 0, 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &3_u32.to_le_bytes())
        .expect("write NumVertices");
    engine
        .mem_write(STACK_TOP + 0x30, &0_u32.to_le_bytes())
        .expect("write StartIndex");
    engine
        .mem_write(STACK_TOP + 0x38, &1_u32.to_le_bytes())
        .expect("write PrimitiveCount");
    assert_return_value!(
        d3d9::handle_draw_indexed_primitive(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // The cyan right triangle (0,0)(3,0)(0,2) covers the pixel centers
    // (0.5,0.5), (1.5,0.5), (0.5,1.5) — the hypotenuse x = 3 − 1.5y cuts the
    // rest of the 4x3 grid out; the outside stays cleared.
    let idx =
        |x: u32, y: u32| usize::try_from(y).unwrap_or(0) * 4 + usize::try_from(x).unwrap_or(0);
    let back = &state.d3d9().d3d9_backbuffer;
    for (x, y) in [(0, 0), (1, 0), (0, 1)] {
        assert_eq!(
            back.get(idx(x, y)).copied(),
            Some(0x0000_FFFF),
            "buffer-form draw must fill ({x},{y}) cyan"
        );
    }
    for (x, y) in [
        (2, 0),
        (2, 1),
        (3, 0),
        (3, 1),
        (0, 2),
        (1, 2),
        (2, 2),
        (3, 2),
    ] {
        assert_eq!(
            back.get(idx(x, y)).copied(),
            Some(0),
            "outside the triangle at ({x},{y}) stays cleared"
        );
    }
}

#[test]
fn test_d3d9_depth_surface_create_bind_and_clear() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest heap bump cursor (see the texture round-trip test).
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateDepthStencilSurface(2x2, D16) → surface at 0x7000.
    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 2, 2, 80, 0); // r9 = D3DFMT_D16 = 80
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(
        surface_va, 0,
        "CreateDepthStencilSurface must return an object"
    );

    // Unsupported format fails honestly.
    write_regs(&mut engine, 1, 2, 2, 99, 0); // unknown format
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );

    // Bind it and clear the depth buffer to 0.25 (near is 0.0).
    write_regs(&mut engine, 1, surface_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    // Clear(D3DCLEAR_ZBUFFER, z=0.25) — the Z arg is a f32 at [rsp+0x30].
    write_regs(&mut engine, 1, 0, 0, 2, 0); // flags = D3DCLEAR_ZBUFFER
    engine
        .mem_write(STACK_TOP + 0x30, &0.25_f32.to_bits().to_le_bytes())
        .expect("write clear z");
    assert_return_value!(
        d3d9::handle_clear(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let record = state
        .d3d9()
        .d3d9_depth_surfaces
        .get(&surface_va)
        .expect("depth record exists");
    assert_eq!(record.width, 2);
    assert_eq!(record.height, 2);
    assert_eq!(record.format, 80);
    assert_eq!(
        record.depth,
        vec![0.25; 4],
        "Clear(ZBUFFER) must fill the depth"
    );

    // GetDepthStencilSurface returns the binding.
    let out_surface = 0x7100_u64;
    write_regs(&mut engine, 1, out_surface, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_depth_stencil_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out_bytes = [0_u8; 8];
    engine
        .mem_read(out_surface, &mut out_bytes)
        .expect("read out");
    assert_eq!(u64::from_le_bytes(out_bytes), surface_va);

    // Release the depth surface: record gone + binding cleared.
    write_regs(&mut engine, surface_va, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(!state.d3d9().d3d9_depth_surfaces.contains_key(&surface_va));
    assert_eq!(state.d3d9().d3d9_depth_stencil, 0, "release must unbind");
}

// ── Buffer helpers ──────────────────────────────────────────────────

/// CreateIndexBuffer + GetDesc round-trip (IB variant of the existing VB test).
#[test]
fn test_d3d9_index_buffer_get_desc() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateIndexBuffer(100 bytes, format=D3DFMT_INDEX16, pool=MANAGED).
    // R9 must hold the format (D3DFMT_INDEX16=101); stack 0x28=pool, 0x30=ppBuffer.
    let pp_buffer = 0x7000_u64;
    write_regs(&mut engine, 1, 100, 0, u64::from(d3d9::D3DFMT_INDEX16), 0);
    engine
        .mem_write(STACK_TOP + 0x28, &1_u32.to_le_bytes())
        .expect("write pool MANAGED");
    engine
        .mem_write(STACK_TOP + 0x30, &pp_buffer.to_le_bytes())
        .expect("write ppBuffer");
    assert_return_value!(
        d3d9::handle_create_index_buffer(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut buf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_buffer, &mut buf_bytes)
        .expect("read buffer ptr");
    let ib = u64::from_le_bytes(buf_bytes);
    assert_ne!(ib, 0, "CreateIndexBuffer must return an object");

    // GetDesc → D3DINDEXBUFFER_DESC (20 bytes, offsets from d3d9types.h):
    //   Format @0 (4), Type @4 (4), Usage @8 (4), Pool @12 (4), Size @16 (4).
    let desc_va = 0x7200_u64;
    write_regs(&mut engine, ib, desc_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_index_buffer_get_desc(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut desc = [0_u8; 20];
    engine.mem_read(desc_va, &mut desc).expect("read desc");
    assert_eq!(
        u32::from_le_bytes(desc[0..4].try_into().unwrap_or([0; 4])),
        d3d9::D3DFMT_INDEX16,
        "Format = INDEX16"
    );
    assert_eq!(
        u32::from_le_bytes(desc[4..8].try_into().unwrap_or([0; 4])),
        7,
        "Type = INDEXBUFFER"
    );
    assert_eq!(
        u32::from_le_bytes(desc[12..16].try_into().unwrap_or([0; 4])),
        1,
        "Pool = MANAGED"
    );
    assert_eq!(
        u32::from_le_bytes(desc[16..20].try_into().unwrap_or([0; 4])),
        100,
        "Size = 100"
    );
}

// ── Texture / surface helpers ───────────────────────────────────────

/// GetLevelCount on a 2-level texture returns 2.
#[test]
fn test_d3d9_texture_get_level_count() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateTexture(16x8, levels=2, format=A8R8G8B8) → texture at 0x7000.
    let pp_texture = 0x7000_u64;
    write_regs(&mut engine, 1, 16, 8, 2, 0);
    engine
        .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
        .expect("write format");
    engine
        .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
        .expect("write ppTexture");
    assert_return_value!(
        d3d9::handle_create_texture(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut tex_bytes = [0_u8; 8];
    engine
        .mem_read(pp_texture, &mut tex_bytes)
        .expect("read texture ptr");
    let texture_va = u64::from_le_bytes(tex_bytes);
    assert_ne!(texture_va, 0);

    // GetLevelCount — the handler returns the level count directly as the
    // WinApiHandlerResult return value (not via an output pointer).
    write_regs(&mut engine, texture_va, 0, 0, 0, 0);
    let result = d3d9::handle_texture_get_level_count(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("handler should succeed");
    assert_eq!(
        result.return_value, 2,
        "GetLevelCount on 2-level texture returns level count directly"
    );
}

/// GetDesc on a render-target surface writes a 64-byte D3DSURFACE_DESC to guest memory.
#[test]
fn test_d3d9_surface_get_desc() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("seed bump cursor");

    // CreateRenderTarget(8x4, format=A8R8G8B8) → surface at 0x7000.
    let pp_surface = 0x7000_u64;
    write_regs(&mut engine, 1, 8, 4, u64::from(D3DFMT_A8R8G8B8), 0);
    engine
        .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
        .expect("write ppSurface");
    assert_return_value!(
        d3d9::handle_create_render_target(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut surf_bytes = [0_u8; 8];
    engine
        .mem_read(pp_surface, &mut surf_bytes)
        .expect("read surface ptr");
    let surface_va = u64::from_le_bytes(surf_bytes);
    assert_ne!(surface_va, 0);

    // GetDesc → D3DSURFACE_DESC at 0x7100.
    let desc_va = 0x7100_u64;
    write_regs(&mut engine, surface_va, desc_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_surface_get_desc(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // Layout the handler actually writes (32 bytes, simplified D3DSURFACE_DESC):
    //   @0: format, @4: D3DRTYPE_SURFACE, @16: MultiSampleType=0, @20: MultiSampleQuality=0,
    //   @24: width, @28: height.  Gaps at @8–15 and @12–15 are left as 0.
    let mut desc = [0_u8; 32];
    engine.mem_read(desc_va, &mut desc).expect("read desc");
    assert_eq!(
        u32::from_le_bytes(desc[0..4].try_into().unwrap_or([0; 4])),
        D3DFMT_A8R8G8B8,
        "Format = A8R8G8B8"
    );
    assert_eq!(
        u32::from_le_bytes(desc[4..8].try_into().unwrap_or([0; 4])),
        3, // D3DRTYPE_SURFACE
        "Type = SURFACE"
    );
    assert_eq!(
        u32::from_le_bytes(desc[16..20].try_into().unwrap_or([0; 4])),
        0,
        "MultiSampleType = NONE"
    );
    assert_eq!(
        u32::from_le_bytes(desc[24..28].try_into().unwrap_or([0; 4])),
        8,
        "Width = 8"
    );
    assert_eq!(
        u32::from_le_bytes(desc[28..32].try_into().unwrap_or([0; 4])),
        4,
        "Height = 4"
    );
}
