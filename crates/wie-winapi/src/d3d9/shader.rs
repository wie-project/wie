use anyhow::{Context, Result};

use super::{D3D_OK, D3DERR_INVALIDCALL, allocate_direct3d_block};
use crate::d3d9_shader::{ShaderKind, ShaderRecord, parse_shader};
use crate::fake_va::{D3d9Iface, PixelShader9Method};
use crate::guest_memory::{read_u32, write_u64 as write_guest_u64};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::texture::fill_com_vtable;
use crate::gdi32::{ArgReg, read_arg};
use crate::kernel32::low_u32;

// ── shader objects ─────────────────────────────────────────────────────
//
// Shader objects are fake COM allocations (vtable + object) like textures;
// the guest holds the object pointer and calls the IUnknown trio through the
// fake vtable. The bytecode is copied host-side at Create* time (the guest
// buffer is transient) and tokenized into ParsedShader.

/// Number of methods in the `IDirect3DPixelShader9` / `IDirect3DVertexShader9`
/// vtables (the IUnknown trio only).
pub const IDIRECT3DSHADER9_METHOD_COUNT: usize = PixelShader9Method::VTABLE_SLOTS;
/// Space reserved for a shader vtable + COM object.
const IDIRECT3DSHADER9_ALLOCATION_SIZE: u64 = 0x40;
/// Offset of the COM object after the 3-entry vtable.
const IDIRECT3DSHADER9_OBJECT_OFFSET: u64 = 0x20;

/// Walk the guest bytecode and copy it host-side.
///
/// Reads DWORD tokens one at a time through the engine (no raw pointer
/// retained); stops at the `end` opcode. The instruction-length walk uses the
/// same per-opcode operand counts as the tokenizer, so malformed streams fail
/// here with `D3DERR_INVALIDCALL` rather than reading past the buffer.
fn read_shader_bytecode(engine: &mut dyn wie_cpu::CpuEngine, bytecode_va: u64) -> Result<Vec<u32>> {
    const MAX_TOKENS: usize = crate::d3d9_shader::MAX_SHADER_TOKENS;
    let mut tokens = Vec::new();
    // Version token.
    let version = read_u32(engine, bytecode_va).context("failed to read shader version")?;
    tokens.push(version);
    let mut offset: u64 = 4;
    let _ = crate::d3d9_shader::decode_shader_version(version)
        .context("unsupported shader version token")?;
    loop {
        if tokens.len() >= MAX_TOKENS {
            anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
        }
        let address = bytecode_va.wrapping_add(offset);
        let token =
            read_u32(engine, address).context("failed to read shader token from guest memory")?;
        offset = offset.wrapping_add(4);
        tokens.push(token);
        let opcode = token & crate::d3d9_shader::OPCODE_FIELD_MASK;
        if opcode == crate::d3d9_shader::D3DSIO_END {
            break;
        }
        if opcode == crate::d3d9_shader::D3DSIO_COMMENT {
            let payload = usize::try_from(
                (token & crate::d3d9_shader::COMMENTSIZE_FIELD_MASK)
                    >> crate::d3d9_shader::COMMENTSIZE_FIELD_SHIFT,
            )
            .context("comment payload too large")?;
            for _ in 0..payload {
                if tokens.len() >= MAX_TOKENS {
                    anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
                }
                let comment_address = bytecode_va.wrapping_add(offset);
                let comment_token = read_u32(engine, comment_address)
                    .context("failed to read shader comment payload")?;
                offset = offset.wrapping_add(4);
                tokens.push(comment_token);
            }
            continue;
        }
        let payload_len =
            crate::d3d9_shader::instruction_payload_len(opcode).context("unknown shader opcode")?;
        // A predicated instruction (bit 28) carries the p0 predicate operand
        // ahead of its normal dst/src operands — the tokenizer counts it the
        // same way, so the walk must add one token here.
        let predicated = token & crate::d3d9_shader::PREDICATED_INSTRUCTION_MASK != 0;
        let operand_count = payload_len
            .checked_add(usize::from(predicated))
            .context("shader operand count overflow")?;
        for _ in 0..operand_count {
            if tokens.len() >= MAX_TOKENS {
                anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
            }
            let operand_address = bytecode_va.wrapping_add(offset);
            let operand_token =
                read_u32(engine, operand_address).context("failed to read shader operand token")?;
            offset = offset.wrapping_add(4);
            tokens.push(operand_token);
        }
    }
    Ok(tokens)
}

/// Allocate a shader object (vtable + COM object) and return its pointer.
fn allocate_shader_object(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    iface: D3d9Iface,
) -> Result<u64> {
    let vtable_address = allocate_direct3d_block(
        engine,
        state,
        IDIRECT3DSHADER9_ALLOCATION_SIZE,
        "IDirect3DShader9",
    );
    if vtable_address == 0 {
        return Ok(0);
    }
    fill_com_vtable(engine, vtable_address, iface, IDIRECT3DSHADER9_METHOD_COUNT)?;
    let object_address = vtable_address
        .checked_add(IDIRECT3DSHADER9_OBJECT_OFFSET)
        .context("IDirect3DShader9 object address overflow")?;
    write_guest_u64(engine, object_address, vtable_address)
        .context("failed to initialize IDirect3DShader9 object")?;
    Ok(object_address)
}

/// Handles `IDirect3DDevice9::CreatePixelShader` (vtable slot 106).
///
/// Copies the guest bytecode (ps_2_0 family only), tokenizes it, and rejects
/// malformed bytecode or shaders whose opcodes the interpreter cannot
/// execute (`D3DERR_INVALIDCALL` — the full instruction set is not yet implemented).
pub fn handle_create_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_shader_impl(ctx, ShaderKind::Pixel, D3d9Iface::PixelShader9)
}

/// Handles `IDirect3DDevice9::CreateVertexShader` (vtable slot 91).
///
/// Accepts vs_2_0 bytecode whose opcodes the vertex-stage interpreter
/// executes; the advanced ops (`m4x4`/`dst`/`lit`/`pow`/… and all flow
/// control) parse but are rejected with `D3DERR_INVALIDCALL` until L5.
pub fn handle_create_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_shader_impl(ctx, ShaderKind::Vertex, D3d9Iface::VertexShader9)
}

/// Shared `CreatePixelShader`/`CreateVertexShader` body: `rcx` = this,
/// `rdx` = bytecode, `r8` = output pointer. The parsed shader is stored and
/// its `def*` constants seed the device constant banks for the shader's
/// stage (see [`seed_constant_banks`]).
fn create_shader_impl(
    ctx: &mut HandlerContext<'_>,
    kind: ShaderKind,
    iface: D3d9Iface,
) -> Result<WinApiHandlerResult> {
    let api_name = match kind {
        ShaderKind::Pixel => "CreatePixelShader",
        ShaderKind::Vertex => "CreateVertexShader",
    };
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, api_name)?;
    let bytecode_va = read_arg(engine, ArgReg::Rdx, api_name)?;
    let pp_shader = read_arg(engine, ArgReg::R8, api_name)?;

    let return_value = if bytecode_va != 0 && pp_shader != 0 {
        let bytecode = read_shader_bytecode(engine, bytecode_va);
        let parsed = bytecode
            .as_deref()
            .ok()
            .and_then(|tokens| parse_shader(tokens).ok());
        match (bytecode, parsed) {
            (Ok(bytecode), Some(parsed)) if parsed.kind == kind && parsed.is_fully_executable() => {
                let object = allocate_shader_object(engine, state, iface)?;
                if object == 0 {
                    D3DERR_INVALIDCALL
                } else {
                    state.d3d9().d3d9_shaders.insert(
                        object,
                        ShaderRecord {
                            handle: object,
                            kind,
                            bytecode,
                            parsed,
                        },
                    );
                    seed_constant_banks(state, object, kind);
                    write_guest_u64(engine, pp_shader, object).with_context(|| {
                        format!(
                            "failed to return IDirect3D{}Shader9 pointer",
                            match kind {
                                ShaderKind::Pixel => "Pixel",
                                ShaderKind::Vertex => "Vertex",
                            }
                        )
                    })?;
                    D3D_OK
                }
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    ctx.finish(return_value)
}

/// Seed the device constant banks from a just-created shader's `def*`
/// pseudo-opcodes (real D3D9 semantics; later `Set*ShaderConstant` calls
/// override). Bank selection follows the shader stage exactly as before the
/// two Create paths were shared: pixel shaders seed `d3d9_ps_constants`,
/// vertex shaders seed the float/bool/int `d3d9_vs_*` banks in that order.
fn seed_constant_banks(state: &mut WinApiState, object: u64, kind: ShaderKind) {
    let d3d = state.d3d9();
    match kind {
        ShaderKind::Pixel => {
            // `def` writes the device constant registers (real D3D9
            // semantics); later SetPixelShaderConstantF calls override.
            let def_constants = d3d
                .d3d9_shaders
                .get(&object)
                .map(|record| record.parsed.constants.clone())
                .unwrap_or_default();
            for (register, value) in &def_constants {
                if let Some(slot) = d3d
                    .d3d9_ps_constants
                    .get_mut(usize::try_from(*register).unwrap_or(usize::MAX))
                {
                    *slot = *value;
                }
            }
        }
        ShaderKind::Vertex => {
            let record = d3d
                .d3d9_shaders
                .get(&object)
                .map(|r| {
                    (
                        r.parsed.constants.clone(),
                        r.parsed.bool_constants.clone(),
                        r.parsed.int_constants.clone(),
                    )
                })
                .unwrap_or_default();
            for (register, value) in &record.0 {
                if let Some(slot) = d3d
                    .d3d9_vs_constants
                    .get_mut(usize::try_from(*register).unwrap_or(usize::MAX))
                {
                    *slot = *value;
                }
            }
            for (register, value) in &record.1 {
                if let Some(slot) = d3d
                    .d3d9_vs_bool_constants
                    .get_mut(usize::try_from(*register).unwrap_or(usize::MAX))
                {
                    *slot = *value;
                }
            }
            for (register, value) in &record.2 {
                if let Some(slot) = d3d
                    .d3d9_vs_int_constants
                    .get_mut(usize::try_from(*register).unwrap_or(usize::MAX))
                {
                    slot[0] = *value;
                }
            }
        }
    }
}

/// Common Set*Shader body: validate the pointer (NULL clears) and bind it.
fn set_shader_binding(state: &mut WinApiState, shader: u64, kind: ShaderKind) {
    let d3d = state.d3d9();
    let known = shader == 0
        || d3d
            .d3d9_shaders
            .get(&shader)
            .is_some_and(|record| record.kind == kind);
    if !known {
        return;
    }
    let d3d = state.d3d9();
    match kind {
        ShaderKind::Pixel => d3d.d3d9_pixel_shader = shader,
        ShaderKind::Vertex => d3d.d3d9_current_vertex_shader = shader,
    }
}

/// Handles `IDirect3DDevice9::SetPixelShader` (vtable slot 107).
pub fn handle_set_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetPixelShader")?;
    let shader = read_arg(engine, ArgReg::Rdx, "SetPixelShader")?;

    set_shader_binding(state, shader, ShaderKind::Pixel);

    let return_value = D3D_OK;
    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetPixelShader` (vtable slot 108).
pub fn handle_get_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetPixelShader")?;
    let pp_shader = read_arg(engine, ArgReg::Rdx, "GetPixelShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_pixel_shader)
            .context("failed to write GetPixelShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::SetVertexShader` (vtable slot 92).
pub fn handle_set_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetVertexShader")?;
    let vertex_shader = read_arg(engine, ArgReg::Rdx, "SetVertexShader")?;

    set_shader_binding(state, vertex_shader, ShaderKind::Vertex);

    let return_value = D3D_OK;
    ctx.finish(return_value)
}

/// Handles `IDirect3DDevice9::GetVertexShader` (vtable slot 93).
pub fn handle_get_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetVertexShader")?;
    let pp_shader = read_arg(engine, ArgReg::Rdx, "GetVertexShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_current_vertex_shader)
            .context("failed to write GetVertexShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    ctx.finish(return_value)
}

/// Common Set*ShaderConstantF body: copy `count` float4s from the guest.
///
/// Reads the raw LE `f32` bytes with the guest memory helpers; registers past
/// the file's end are dropped (the writes are clamped to the file).
fn set_shader_constant_f(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [[f32; 4]],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let float_address = data_va.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        engine
            .mem_read(float_address, &mut bytes)
            .context("failed to read shader constant data")?;
        let mut chunks = [[0_u8; 4]; 4];
        for (chunk, byte_chunk) in chunks.iter_mut().zip(bytes.chunks_exact(4)) {
            chunk.copy_from_slice(byte_chunk);
        }
        for (channel, chunk) in chunks.into_iter().enumerate() {
            if let Some(slot_channel) = slot.get_mut(channel) {
                *slot_channel = f32::from_le_bytes(chunk);
            }
        }
    }
    Ok(())
}

/// Common Get*ShaderConstantF body: copy `count` float4s to the guest.
fn get_shader_constant_f(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &[[f32; 4]],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_va.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        for (channel, byte_chunk) in bytes.chunks_exact_mut(4).enumerate() {
            let chunk = value.get(channel).copied().unwrap_or(0.0).to_le_bytes();
            byte_chunk.copy_from_slice(&chunk);
        }
        engine
            .mem_write(address, &bytes)
            .context("failed to write shader constant data")?;
    }
    Ok(())
}

/// Handles `IDirect3DDevice9::SetPixelShaderConstantF` (vtable slot 109).
pub fn handle_set_pixel_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetPixelShaderConstantF")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "SetPixelShaderConstantF")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_ps_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetPixelShaderConstantF` (vtable slot 110).
pub fn handle_get_pixel_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetPixelShaderConstantF")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "GetPixelShaderConstantF")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_ps_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::SetVertexShaderConstantF` (vtable slot 94).
///
/// Stores into the vs_2_0 constant file (256 float4s); the values are used
/// when vertex shaders execute.
pub fn handle_set_vertex_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetVertexShaderConstantF")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "SetVertexShaderConstantF")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_vs_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantF` (vtable slot 95).
pub fn handle_get_vertex_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetVertexShaderConstantF")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "GetVertexShaderConstantF")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_vs_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Common Set*ShaderConstantI body: copy `count` int4s from the guest into an
/// integer register file (clamped to the file end, like the float form).
fn set_shader_constant_i(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [[i32; 4]],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let int_address = data_va.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        engine
            .mem_read(int_address, &mut bytes)
            .context("failed to read shader integer constant data")?;
        for (channel, byte_chunk) in bytes.chunks_exact(4).enumerate() {
            if let Some(slot_channel) = slot.get_mut(channel) {
                *slot_channel = i32::from_le_bytes(byte_chunk.try_into().unwrap_or([0; 4]));
            }
        }
    }
    Ok(())
}

/// Common Get*ShaderConstantI body: copy `count` int4s back to the guest.
fn get_shader_constant_i(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &[[i32; 4]],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_va.wrapping_add(u64::from(index).wrapping_mul(16));
        let mut bytes = [0_u8; 16];
        for (channel, byte_chunk) in bytes.chunks_exact_mut(4).enumerate() {
            let chunk = value.get(channel).copied().unwrap_or(0).to_le_bytes();
            byte_chunk.copy_from_slice(&chunk);
        }
        engine
            .mem_write(address, &bytes)
            .context("failed to write shader integer constant data")?;
    }
    Ok(())
}

/// Common Set*ShaderConstantB body: copy `count` BOOLs (4 bytes each,
/// TRUE = nonzero) into a boolean register file.
fn set_shader_constant_b(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [bool],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let bool_address = data_va.wrapping_add(u64::from(index).wrapping_mul(4));
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(bool_address, &mut bytes)
            .context("failed to read shader boolean constant data")?;
        *slot = u32::from_le_bytes(bytes) != 0;
    }
    Ok(())
}

/// Common Get*ShaderConstantB body: copy `count` BOOLs back to the guest.
fn get_shader_constant_b(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &[bool],
    start_register: u32,
    data_va: u64,
    count: u32,
) -> Result<()> {
    if data_va == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_va.wrapping_add(u64::from(index).wrapping_mul(4));
        let raw = if *value { 1_u32 } else { 0_u32 };
        engine
            .mem_write(address, &raw.to_le_bytes())
            .context("failed to write shader boolean constant data")?;
    }
    Ok(())
}

/// Handles `IDirect3DDevice9::SetVertexShaderConstantI` (vtable slot 96).
///
/// Stores into the vs_2_0 integer constant file `i0..i3` (int4 per register);
/// the `loop`/`rep` instructions read these as their iteration specs.
pub fn handle_set_vertex_shader_constant_i(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetVertexShaderConstantI")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "SetVertexShaderConstantI")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    set_shader_constant_i(
        engine,
        &mut d3d.d3d9_vs_int_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantI` (vtable slot 97).
pub fn handle_get_vertex_shader_constant_i(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetVertexShaderConstantI")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "GetVertexShaderConstantI")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    get_shader_constant_i(
        engine,
        &d3d.d3d9_vs_int_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::SetVertexShaderConstantB` (vtable slot 98).
///
/// Stores into the vs_2_0 boolean constant file `b0..b15`; the `if`/`callnz`
/// instructions read these as their conditions.
pub fn handle_set_vertex_shader_constant_b(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "SetVertexShaderConstantB")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "SetVertexShaderConstantB")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    set_shader_constant_b(
        engine,
        &mut d3d.d3d9_vs_bool_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantB` (vtable slot 99).
pub fn handle_get_vertex_shader_constant_b(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = read_arg(engine, ArgReg::Rcx, "GetVertexShaderConstantB")?;
    let start_register = low_u32(engine.read_rdx()?, "start register")?;
    let data_va = read_arg(engine, ArgReg::R8, "GetVertexShaderConstantB")?;
    let count = low_u32(engine.read_r9()?, "count")?;

    let d3d = state.d3d9();
    get_shader_constant_b(
        engine,
        &d3d.d3d9_vs_bool_constants,
        start_register,
        data_va,
        count,
    )?;

    ctx.finish(D3D_OK)
}

/// Handles `IDirect3DPixelShader9::Release` (vtable slot 2).
pub fn handle_pixel_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DPixelShader9::Release")?;

    let return_value = if state.d3d9().d3d9_shaders.remove(&this_pointer).is_some() {
        if state.d3d9().d3d9_pixel_shader == this_pointer {
            state.d3d9().d3d9_pixel_shader = 0;
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DSHADER9_OBJECT_OFFSET);
        let _ = state.heap_state.heap.free_coherent(engine, vtable);
        1
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Handles `IDirect3DVertexShader9::Release` (vtable slot 2).
pub fn handle_vertex_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DVertexShader9::Release")?;

    let return_value = if state.d3d9().d3d9_shaders.remove(&this_pointer).is_some() {
        if state.d3d9().d3d9_current_vertex_shader == this_pointer {
            state.d3d9().d3d9_current_vertex_shader = 0;
        }
        let vtable = this_pointer.saturating_sub(IDIRECT3DSHADER9_OBJECT_OFFSET);
        let _ = state.heap_state.heap.free_coherent(engine, vtable);
        1
    } else {
        0
    };

    ctx.finish(return_value)
}
