//! D3D9 typed render / stage state tests: render-state, fog / alpha / scissor state, transform math, scissor rects, and sampler stage state round-trips.
use super::*;

#[test]
fn test_d3d9_render_state_typed_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // SetRenderState(this, state, value) through the register ABI.
    let set = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32, value: u32| {
        write_regs(engine, 1, u64::from(state_id), u64::from(value), 0, 0);
        assert_return_value!(
            d3d9::handle_set_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    // GetRenderState(this, state, &out) returns the typed value.
    let get = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, u64::from(state_id), out, 0, 0);
        assert_return_value!(
            d3d9::handle_get_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetRenderState output");
        u32::from_le_bytes(bytes)
    };

    // The D3D9 fixed-function defaults match the fragment-stage fallbacks.
    let rs = state.d3d9();
    assert!(!rs.d3d9_render_state.alpha_blend_enable);
    assert!(rs.d3d9_render_state.z_write_enable);
    assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 0);
    assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
    assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 2);
    assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 1);
    assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

    // Set + struct check for every supported D3DRS_*.
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ALPHABLENDENABLE,
        1,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ZENABLE,
        1,
    );
    set(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC, 4);
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_SRCBLEND,
        5,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_DESTBLEND,
        6,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_BLENDOP,
        1,
    );
    set(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DRS_ZWRITEENABLE,
        0,
    );
    let rs = state.d3d9();
    assert!(rs.d3d9_render_state.alpha_blend_enable);
    assert!(!rs.d3d9_render_state.z_write_enable);
    assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 1);
    assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
    assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 5);
    assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 6);
    assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

    // GetRenderState round-trips each supported state.
    assert_eq!(
        get(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ALPHABLENDENABLE
        ),
        1
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZENABLE),
        1
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC),
        4
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
        5
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_DESTBLEND),
        6
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_BLENDOP),
        1
    );
    assert_eq!(
        get(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ZWRITEENABLE
        ),
        0
    );

    // L3 raw-value layer: an unmodeled D3DRS_* keeps the last-set value —
    // GetRenderState round-trips it (no more "0 for ignored").
    set(&mut engine, &mut state, 0x1FF, 7);
    assert_eq!(get(&mut engine, &mut state, 0x1FF), 7);
    // Setting the same unmodeled state again overwrites the stored value.
    set(&mut engine, &mut state, 0x1FF, 9);
    assert_eq!(get(&mut engine, &mut state, 0x1FF), 9);
    // A never-set unmodeled state reads 0 (D3D9's default for unused states).
    assert_eq!(get(&mut engine, &mut state, 0x1FE), 0);
    // L3 validation: an out-of-range enum value is the honest
    // D3DERR_INVALIDCALL and the state keeps its previous value.
    write_regs(
        &mut engine,
        1,
        u64::from(crate::d3d9_render::D3DRS_SRCBLEND),
        0xDEAD,
        0,
        0,
    );
    assert_return_value!(
        d3d9::handle_set_render_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c // D3DERR_INVALIDCALL
    );
    assert_eq!(
        get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
        5,
        "the rejected value must not overwrite the last legal one"
    );
}

#[test]
fn test_d3d9_fog_alpha_scissor_render_state_round_trip() {
    use crate::d3d9_render::{
        D3DCMP_GREATER, D3DFOG_LINEAR, D3DRS_ALPHAFUNC, D3DRS_ALPHAREF, D3DRS_ALPHATESTENABLE,
        D3DRS_FOGCOLOR, D3DRS_FOGDENSITY, D3DRS_FOGENABLE, D3DRS_FOGEND, D3DRS_FOGSTART,
        D3DRS_FOGTABLEMODE, D3DRS_FOGVERTEXMODE, D3DRS_SCISSORTESTENABLE,
    };
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let set = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32, value: u32| {
        write_regs(engine, 1, u64::from(state_id), u64::from(value), 0, 0);
        assert_return_value!(
            d3d9::handle_set_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    let get = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, u64::from(state_id), out, 0, 0);
        assert_return_value!(
            d3d9::handle_get_render_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetRenderState output");
        u32::from_le_bytes(bytes)
    };

    // The L3 defaults (D3D9 fixed-function).
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGENABLE), 0);
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGCOLOR), 0);
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGSTART),
        0.0_f32.to_bits()
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGEND),
        1.0_f32.to_bits()
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGDENSITY),
        1.0_f32.to_bits()
    );
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGTABLEMODE), 0);
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGVERTEXMODE), 0);
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHATESTENABLE), 0);
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHAFUNC), 8); // D3DCMP_ALWAYS
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHAREF), 0);
    assert_eq!(get(&mut engine, &mut state, D3DRS_SCISSORTESTENABLE), 0);

    // Set + round-trip every fog/alpha/scissor state.
    set(&mut engine, &mut state, D3DRS_FOGENABLE, 1);
    set(&mut engine, &mut state, D3DRS_FOGCOLOR, 0x00FF_FF00);
    set(&mut engine, &mut state, D3DRS_FOGSTART, 0.25_f32.to_bits());
    set(&mut engine, &mut state, D3DRS_FOGEND, 0.75_f32.to_bits());
    set(&mut engine, &mut state, D3DRS_FOGDENSITY, 0.5_f32.to_bits());
    set(&mut engine, &mut state, D3DRS_FOGTABLEMODE, D3DFOG_LINEAR);
    set(&mut engine, &mut state, D3DRS_FOGVERTEXMODE, D3DFOG_LINEAR);
    set(&mut engine, &mut state, D3DRS_ALPHATESTENABLE, 1);
    set(&mut engine, &mut state, D3DRS_ALPHAFUNC, D3DCMP_GREATER);
    set(&mut engine, &mut state, D3DRS_ALPHAREF, 0x40);
    set(&mut engine, &mut state, D3DRS_SCISSORTESTENABLE, 1);
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGENABLE), 1);
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGCOLOR), 0x00FF_FF00);
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGSTART),
        0.25_f32.to_bits()
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGEND),
        0.75_f32.to_bits()
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGDENSITY),
        0.5_f32.to_bits()
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGTABLEMODE),
        D3DFOG_LINEAR
    );
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_FOGVERTEXMODE),
        D3DFOG_LINEAR
    );
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHATESTENABLE), 1);
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_ALPHAFUNC),
        D3DCMP_GREATER
    );
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHAREF), 0x40);
    assert_eq!(get(&mut engine, &mut state, D3DRS_SCISSORTESTENABLE), 1);

    // The typed struct mirrors the round-trip (the fragment stage reads it).
    let rs = state.d3d9().d3d9_render_state;
    assert!(rs.fog_enable);
    assert_eq!(rs.fog_color, 0x00FF_FF00);
    assert_eq!(rs.fog_start.to_bits(), 0.25_f32.to_bits());
    assert_eq!(rs.fog_end.to_bits(), 0.75_f32.to_bits());
    assert_eq!(rs.fog_density.to_bits(), 0.5_f32.to_bits());
    assert_eq!(rs.fog_table_mode, D3DFOG_LINEAR);
    assert_eq!(rs.fog_vertex_mode, D3DFOG_LINEAR);
    assert!(rs.alpha_test_enable);
    assert_eq!(rs.alpha_func.as_u32(), D3DCMP_GREATER);
    assert_eq!(rs.alpha_ref, 0x40);
    assert!(rs.scissor_test_enable);

    // Validation: out-of-range values fail for the new states too.
    for (state_id, bad) in [
        (D3DRS_FOGENABLE, 2_u32),
        (D3DRS_ALPHATESTENABLE, 2),
        (D3DRS_SCISSORTESTENABLE, 2),
        (D3DRS_ALPHAFUNC, 9),
        (D3DRS_ALPHAREF, 256),
        (D3DRS_FOGTABLEMODE, 4),
        (D3DRS_FOGVERTEXMODE, 4),
    ] {
        write_regs(&mut engine, 1, u64::from(state_id), u64::from(bad), 0, 0);
        assert_return_value!(
            d3d9::handle_set_render_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x8876_086c // D3DERR_INVALIDCALL
        );
    }
    // The rejected sets left the earlier values intact.
    assert_eq!(get(&mut engine, &mut state, D3DRS_FOGENABLE), 1);
    assert_eq!(
        get(&mut engine, &mut state, D3DRS_ALPHAFUNC),
        D3DCMP_GREATER
    );
    assert_eq!(get(&mut engine, &mut state, D3DRS_ALPHAREF), 0x40);
}

#[test]
fn test_d3d9_get_and_multiply_transform() {
    use crate::d3d9_render::{D3DTS_TEXTURE0, D3DTS_WORLD};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Write a known column-major matrix at 0x6000: scale(2,3,0.5).
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
    let read_matrix = |engine: &mut IcedCpu, va: u64| -> [f32; 16] {
        let mut out = [0.0_f32; 16];
        let mut bytes = [0_u8; 64];
        engine
            .mem_read(va, &mut bytes)
            .expect("read D3DMATRIX output");
        for (index, slot) in out.iter_mut().enumerate() {
            let start = index.saturating_mul(4);
            let raw: [u8; 4] = bytes
                .get(start..start.saturating_add(4))
                .and_then(|s| s.try_into().ok())
                .unwrap_or([0; 4]);
            *slot = f32::from_le_bytes(raw);
        }
        out
    };

    // MultiplyTransform(WORLD, M) — the world starts at identity, so the
    // stored world must become exactly M (current × M).
    write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_multiply_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_world_matrix.map(f32::to_bits),
        matrix.map(f32::to_bits),
        "MultiplyTransform on identity must yield the input matrix"
    );

    // GetTransform(WORLD) writes it back bit-exact.
    let out_va = 0x6400_u64;
    write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), out_va, 0, 0);
    assert_return_value!(
        d3d9::handle_get_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        read_matrix(&mut engine, out_va).map(f32::to_bits),
        matrix.map(f32::to_bits),
        "GetTransform must round-trip the multiplied matrix"
    );

    // A second MultiplyTransform concatenates: world = M × M.
    write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_multiply_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let world = state.d3d9().d3d9_world_matrix;
    let expected = crate::d3d9_render::mat4_mul(&matrix, &matrix);
    assert_eq!(
        world.map(f32::to_bits),
        expected.map(f32::to_bits),
        "the second multiply must concatenate current × M"
    );

    // D3DTS_TEXTURE0..7 store + round-trip (the actual texgen is a later slice).
    write_regs(&mut engine, 1, u64::from(D3DTS_TEXTURE0), matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_set_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, 1, u64::from(D3DTS_TEXTURE0), out_va, 0, 0);
    assert_return_value!(
        d3d9::handle_get_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        read_matrix(&mut engine, out_va).map(f32::to_bits),
        matrix.map(f32::to_bits),
        "GetTransform must round-trip D3DTS_TEXTURE0"
    );
    // D3DTS_TEXTURE1 is still the identity default (TEXTURE1 = TEXTURE0 + 1).
    write_regs(&mut engine, 1, u64::from(D3DTS_TEXTURE0 + 1), out_va, 0, 0);
    assert_return_value!(
        d3d9::handle_get_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        read_matrix(&mut engine, out_va).map(f32::to_bits),
        crate::d3d9_render::IDENTITY.map(f32::to_bits),
        "D3DTS_TEXTURE1 defaults to identity"
    );

    // An unknown transform state is D3DERR_INVALIDCALL.
    write_regs(&mut engine, 1, 0xDEAD, out_va, 0, 0);
    assert_return_value!(
        d3d9::handle_get_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c
    );
    write_regs(&mut engine, 1, 0xDEAD, matrix_va, 0, 0);
    assert_return_value!(
        d3d9::handle_multiply_transform(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c
    );
}

#[test]
fn test_d3d9_set_scissor_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A RECT {10, 20, 130, 140} (4 x i32) at 0x6000.
    let rect_va = 0x6000_u64;
    for (i, v) in [10_i32, 20, 130, 140].iter().enumerate() {
        engine
            .mem_write(
                rect_va + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write RECT field");
    }
    write_regs(&mut engine, 1, rect_va, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_scissor_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert_eq!(
        state.d3d9().d3d9_scissor_rect,
        Some(crate::gdi32::IRect {
            left: 10,
            top: 20,
            right: 130,
            bottom: 140
        })
    );
    // NULL rect is D3DERR_INVALIDCALL and leaves the stored rect intact.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_scissor_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x8876_086c
    );
    assert_eq!(
        state.d3d9().d3d9_scissor_rect,
        Some(crate::gdi32::IRect {
            left: 10,
            top: 20,
            right: 130,
            bottom: 140
        })
    );
}

#[test]
fn test_d3d9_stage_state_typed_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // SetTextureStageState(this, stage=0, slot, value) via the register ABI.
    let set_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
        write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
        assert_return_value!(
            d3d9::handle_set_texture_stage_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    // SetSamplerState(this, sampler=0, slot, value) via the register ABI.
    let set_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
        write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
        assert_return_value!(
            d3d9::handle_set_sampler_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
    };
    let get_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, 0, u64::from(slot), out, 0);
        assert_return_value!(
            d3d9::handle_get_texture_stage_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetTextureStageState output");
        u32::from_le_bytes(bytes)
    };
    let get_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
        let out = 0x7400_u64;
        write_regs(engine, 1, 0, u64::from(slot), out, 0);
        assert_return_value!(
            d3d9::handle_get_sampler_state(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            0
        );
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(out, &mut bytes)
            .expect("read GetSamplerState output");
        u32::from_le_bytes(bytes)
    };

    // The stage-0 defaults match the legacy resolve fallbacks.
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_MODULATE);
    assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_POINT);
    assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_WRAP);

    // Set TSS slots → typed fields.
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLOROP,
        crate::d3d9_render::D3DTOP_SELECTARG1,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLORARG1,
        crate::d3d9_render::D3DTA_TEXTURE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_COLORARG2,
        crate::d3d9_render::D3DTA_DIFFUSE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAOP,
        crate::d3d9_render::D3DTOP_MODULATE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAARG1,
        crate::d3d9_render::D3DTA_TEXTURE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_ALPHAARG2,
        crate::d3d9_render::D3DTA_DIFFUSE,
    );
    set_tss(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DTSS_TEXCOORDINDEX,
        3,
    );
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_SELECTARG1);
    assert_eq!(stage.color_arg1, crate::d3d9_render::D3DTA_TEXTURE);
    assert_eq!(stage.color_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
    assert_eq!(stage.alpha_op, crate::d3d9_render::D3DTOP_MODULATE);
    assert_eq!(stage.alpha_arg1, crate::d3d9_render::D3DTA_TEXTURE);
    assert_eq!(stage.alpha_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
    assert_eq!(stage.tex_coord_index, 3);

    // Set sampler slots → typed fields (D3DSAMP_* namespace).
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_MAGFILTER,
        crate::d3d9_render::D3DTEXF_LINEAR,
    );
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_ADDRESSU,
        crate::d3d9_render::D3DTADDRESS_CLAMP,
    );
    set_samp(
        &mut engine,
        &mut state,
        crate::d3d9_render::D3DSAMP_ADDRESSV,
        crate::d3d9_render::D3DTADDRESS_CLAMP,
    );
    let stage = state
        .d3d9()
        .d3d9_stage_states
        .first()
        .expect("stage 0 exists");
    assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_LINEAR);
    assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_CLAMP);
    assert_eq!(stage.address_v, crate::d3d9_render::D3DTADDRESS_CLAMP);

    // Get* round-trips the typed slots.
    assert_eq!(
        get_tss(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
        crate::d3d9_render::D3DTOP_SELECTARG1
    );
    assert_eq!(
        get_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_COLORARG2
        ),
        crate::d3d9_render::D3DTA_DIFFUSE
    );
    assert_eq!(
        get_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_MAGFILTER
        ),
        crate::d3d9_render::D3DTEXF_LINEAR
    );
    assert_eq!(
        get_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSU
        ),
        crate::d3d9_render::D3DTADDRESS_CLAMP
    );

    // Unmodeled slots are preserved verbatim for the Get* round-trips.
    set_tss(&mut engine, &mut state, 99, 0xAB);
    set_samp(&mut engine, &mut state, 88, 0xCD);
    assert_eq!(get_tss(&mut engine, &mut state, 99), 0xAB);
    assert_eq!(get_samp(&mut engine, &mut state, 88), 0xCD);
    // The two namespaces stay separate despite the colliding constants:
    // TSS slot 1 is COLOROP (set to SELECTARG1 above), sampler slot 1 is
    // ADDRESSU (set to CLAMP above).
    assert_eq!(
        get_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSU
        ),
        crate::d3d9_render::D3DTOP_SELECTARG1
    );
    assert_eq!(
        get_samp(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
        crate::d3d9_render::D3DTADDRESS_CLAMP
    );
}

// ── Stream / index binding ──────────────────────────────────────────

/// SetStreamSource(NULL) + GetStreamSource returns NULL / zero stride.
#[test]
fn test_d3d9_set_get_stream_source_null() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 1, 0, 0, 0, 0); // stream=0, data=NULL, offset=0, stride=0
    engine
        .mem_write(STACK_TOP + 0x28, &0_u32.to_le_bytes())
        .expect("write stride 0");
    assert_return_value!(
        d3d9::handle_set_stream_source(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let pp_data = 0x7400_u64;
    let p_offset = 0x7408_u64;
    let p_stride = 0x7410_u64;
    write_regs(&mut engine, 1, 0, pp_data, p_offset, 0);
    engine.mem_write(p_stride, &0_u32.to_le_bytes()).ok();
    assert_return_value!(
        d3d9::handle_get_stream_source(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out = [0_u8; 8];
    engine.mem_read(pp_data, &mut out).expect("read buffer");
    assert_eq!(u64::from_le_bytes(out), 0, "stream 0 NULL → buffer VA 0");
    let mut off = [0_u8; 4];
    engine.mem_read(p_offset, &mut off).expect("read offset");
    assert_eq!(u32::from_le_bytes(off), 0, "stream 0 NULL → offset 0");
    let mut strd = [0_u8; 4];
    engine.mem_read(p_stride, &mut strd).expect("read stride");
    assert_eq!(u32::from_le_bytes(strd), 0, "stream 0 NULL → stride 0");
}

/// SetIndices(NULL) + GetIndices returns NULL.
#[test]
fn test_d3d9_set_get_indices_null() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_indices(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let pp_index = 0x7400_u64;
    write_regs(&mut engine, 1, pp_index, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_indices(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out = [0_u8; 8];
    engine
        .mem_read(pp_index, &mut out)
        .expect("read index buffer");
    assert_eq!(
        u64::from_le_bytes(out),
        0,
        "SetIndices(NULL) → GetIndices returns NULL"
    );
}

// ── Shader slot binding ─────────────────────────────────────────────

/// SetVertexShader(NULL) + GetVertexShader returns NULL.
#[test]
fn test_d3d9_set_get_vertex_shader_slot_null() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_vertex_shader(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let pp_shader = 0x7400_u64;
    write_regs(&mut engine, 1, pp_shader, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_vertex_shader(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out = [0_u8; 8];
    engine.mem_read(pp_shader, &mut out).expect("read shader");
    assert_eq!(
        u64::from_le_bytes(out),
        0,
        "SetVertexShader(NULL) → GetVertexShader returns NULL"
    );
}

/// SetPixelShader(NULL) + GetPixelShader returns NULL.
#[test]
fn test_d3d9_set_get_pixel_shader_slot_null() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_set_pixel_shader(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let pp_shader = 0x7400_u64;
    write_regs(&mut engine, 1, pp_shader, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_get_pixel_shader(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    let mut out = [0_u8; 8];
    engine.mem_read(pp_shader, &mut out).expect("read shader");
    assert_eq!(
        u64::from_le_bytes(out),
        0,
        "SetPixelShader(NULL) → GetPixelShader returns NULL"
    );
}

// ── Shader float constants ──────────────────────────────────────────

/// SetPixelShaderConstantF + GetPixelShaderConstantF round-trip: one float4 slot.
#[test]
fn test_d3d9_set_get_pixel_shader_constant_f_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Write {1.0, 2.0, 3.0, 4.0} at register 5 via SetPixelShaderConstantF.
    let data_va = 0x6000_u64;
    let values: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
    for (i, v) in values.iter().enumerate() {
        engine
            .mem_write(
                data_va + u64::try_from(i).unwrap_or(0) * 4,
                &v.to_le_bytes(),
            )
            .expect("write constant");
    }
    write_regs(&mut engine, 1, 5, data_va, 1, 0); // start=5, count=1
    assert_return_value!(
        d3d9::handle_set_pixel_shader_constant_f(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // Read it back via GetPixelShaderConstantF.
    let out_va = 0x7400_u64;
    write_regs(&mut engine, 1, 5, out_va, 1, 0);
    assert_return_value!(
        d3d9::handle_get_pixel_shader_constant_f(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let mut out = [0_u8; 16];
    engine
        .mem_read(out_va, &mut out)
        .expect("read constants back");
    let retrieved: [f32; 4] = [
        f32::from_le_bytes(out[0..4].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[4..8].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[8..12].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[12..16].try_into().unwrap_or([0; 4])),
    ];
    assert_eq!(
        retrieved, values,
        "GetPixelShaderConstantF must return what SetPixelShaderConstantF wrote"
    );
}

/// SetVertexShaderConstantF + GetVertexShaderConstantF round-trip: two float4 slots.
#[test]
fn test_d3d9_set_get_vertex_shader_constant_f_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Write two float4s starting at register 10.
    let data_va = 0x6000_u64;
    let values: [[f32; 4]; 2] = [[1.0, 2.0, 3.0, 4.0], [-1.0, 0.5, 0.0, 1.0]];
    for (reg, vec) in values.iter().enumerate() {
        for (i, v) in vec.iter().enumerate() {
            engine
                .mem_write(
                    data_va
                        + u64::try_from(reg).unwrap_or(0) * 16
                        + u64::try_from(i).unwrap_or(0) * 4,
                    &v.to_le_bytes(),
                )
                .expect("write constant");
        }
    }
    write_regs(&mut engine, 1, 10, data_va, 2, 0); // start=10, count=2
    assert_return_value!(
        d3d9::handle_set_vertex_shader_constant_f(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // Read back the second slot via GetVertexShaderConstantF.
    let out_va = 0x7400_u64;
    write_regs(&mut engine, 1, 11, out_va, 1, 0); // start=11, count=1
    assert_return_value!(
        d3d9::handle_get_vertex_shader_constant_f(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    let mut out = [0_u8; 16];
    engine
        .mem_read(out_va, &mut out)
        .expect("read constants back");
    let retrieved: [f32; 4] = [
        f32::from_le_bytes(out[0..4].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[4..8].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[8..12].try_into().unwrap_or([0; 4])),
        f32::from_le_bytes(out[12..16].try_into().unwrap_or([0; 4])),
    ];
    assert_eq!(
        retrieved, values[1],
        "GetVertexShaderConstantF(slot 11) must return the written value"
    );
}

// ── Shader release ──────────────────────────────────────────────────

/// Release of an unknown pixel-shader pointer returns 0 (not in the map).
#[test]
fn test_d3d9_pixel_shader_release_unknown_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 0xDEAD_BEEF, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_pixel_shader_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
}

/// Release of an unknown vertex-shader pointer returns 0 (not in the map).
#[test]
fn test_d3d9_vertex_shader_release_unknown_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    write_regs(&mut engine, 0xDEAD_BEEF, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_vertex_shader_release(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
}
