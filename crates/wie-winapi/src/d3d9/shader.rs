use anyhow::{Context, Result};

use super::{D3D_OK, D3DERR_INVALIDCALL, allocate_direct3d_block};
use crate::d3d9_shader::{ShaderKind, ShaderRecord, parse_shader};
use crate::fake_va::{D3d9Iface, PixelShader9Method};
use crate::guest_memory::{read_u32, write_u64 as write_guest_u64};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::texture::fill_com_vtable;

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
fn read_shader_bytecode(
    engine: &mut dyn wie_cpu::CpuEngine,
    bytecode_ptr: u64,
) -> Result<Vec<u32>> {
    const MAX_TOKENS: usize = crate::d3d9_shader::MAX_SHADER_TOKENS;
    let mut tokens = Vec::new();
    // Version token.
    let version = read_u32(engine, bytecode_ptr).context("failed to read shader version")?;
    tokens.push(version);
    let mut offset: u64 = 4;
    let _ = crate::d3d9_shader::decode_shader_version(version)
        .context("unsupported shader version token")?;
    loop {
        if tokens.len() >= MAX_TOKENS {
            anyhow::bail!("shader exceeds {MAX_TOKENS} tokens");
        }
        let address = bytecode_ptr.wrapping_add(offset);
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
                let comment_address = bytecode_ptr.wrapping_add(offset);
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
            let operand_address = bytecode_ptr.wrapping_add(offset);
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
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreatePixelShader")?;
    let bytecode_ptr = engine
        .read_rdx()
        .context("failed to read RDX for CreatePixelShader")?;
    let pp_shader = engine
        .read_r8()
        .context("failed to read R8 for CreatePixelShader")?;

    let return_value = if bytecode_ptr != 0 && pp_shader != 0 {
        let bytecode = read_shader_bytecode(engine, bytecode_ptr);
        let parsed = bytecode
            .as_deref()
            .ok()
            .and_then(|tokens| parse_shader(tokens).ok());
        match (bytecode, parsed) {
            (Ok(bytecode), Some(parsed))
                if parsed.kind == ShaderKind::Pixel && parsed.is_fully_executable() =>
            {
                let object = allocate_shader_object(engine, state, D3d9Iface::PixelShader9)?;
                if object == 0 {
                    D3DERR_INVALIDCALL
                } else {
                    state.d3d9().d3d9_shaders.insert(
                        object,
                        ShaderRecord {
                            handle: object,
                            kind: ShaderKind::Pixel,
                            bytecode,
                            parsed,
                        },
                    );
                    // `def` writes the device constant registers (real D3D9
                    // semantics); later SetPixelShaderConstantF calls override.
                    let d3d = state.d3d9();
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
                    write_guest_u64(engine, pp_shader, object)
                        .context("failed to return IDirect3DPixelShader9 pointer")?;
                    D3D_OK
                }
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreatePixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::CreateVertexShader` (vtable slot 91).
///
/// Accepts vs_2_0 bytecode whose opcodes the vertex-stage interpreter
/// executes; the advanced ops (`m4x4`/`dst`/`lit`/`pow`/… and all flow
/// control) parse but are rejected with `D3DERR_INVALIDCALL` until L5.
pub fn handle_create_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for CreateVertexShader")?;
    let bytecode_ptr = engine
        .read_rdx()
        .context("failed to read RDX for CreateVertexShader")?;
    let pp_shader = engine
        .read_r8()
        .context("failed to read R8 for CreateVertexShader")?;

    let return_value = if bytecode_ptr != 0 && pp_shader != 0 {
        let bytecode = read_shader_bytecode(engine, bytecode_ptr);
        let parsed = bytecode
            .as_deref()
            .ok()
            .and_then(|tokens| parse_shader(tokens).ok());
        match (bytecode, parsed) {
            (Ok(bytecode), Some(parsed))
                if parsed.kind == ShaderKind::Vertex && parsed.is_fully_executable() =>
            {
                let object = allocate_shader_object(engine, state, D3d9Iface::VertexShader9)?;
                if object == 0 {
                    D3DERR_INVALIDCALL
                } else {
                    state.d3d9().d3d9_shaders.insert(
                        object,
                        ShaderRecord {
                            handle: object,
                            kind: ShaderKind::Vertex,
                            bytecode,
                            parsed,
                        },
                    );
                    // `def`/`defb`/`defi` write the device constant registers
                    // at Create time (real D3D9 semantics — the same as the
                    // pixel-shader `def` path); later Set*ShaderConstant
                    // calls override.
                    let d3d = state.d3d9();
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
                    write_guest_u64(engine, pp_shader, object)
                        .context("failed to return IDirect3DVertexShader9 pointer")?;
                    D3D_OK
                }
            }
            _ => D3DERR_INVALIDCALL,
        }
    } else {
        D3DERR_INVALIDCALL
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetPixelShader")?;
    let shader = engine
        .read_rdx()
        .context("failed to read RDX for SetPixelShader")?;

    set_shader_binding(state, shader, ShaderKind::Pixel);

    let return_value = D3D_OK;
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetPixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::GetPixelShader` (vtable slot 108).
pub fn handle_get_pixel_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetPixelShader")?;
    let pp_shader = engine
        .read_rdx()
        .context("failed to read RDX for GetPixelShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_pixel_shader)
            .context("failed to write GetPixelShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetPixelShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::SetVertexShader` (vtable slot 92).
pub fn handle_set_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShader")?;
    let vertex_shader = engine
        .read_rdx()
        .context("failed to read RDX for SetVertexShader")?;

    set_shader_binding(state, vertex_shader, ShaderKind::Vertex);

    let return_value = D3D_OK;
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShader` (vtable slot 93).
pub fn handle_get_vertex_shader(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShader")?;
    let pp_shader = engine
        .read_rdx()
        .context("failed to read RDX for GetVertexShader")?;

    let return_value = if pp_shader != 0 {
        write_guest_u64(engine, pp_shader, state.d3d9().d3d9_current_vertex_shader)
            .context("failed to write GetVertexShader output")?;
        D3D_OK
    } else {
        D3DERR_INVALIDCALL
    };
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetVertexShader")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Common Set*ShaderConstantF body: copy `count` float4s from the guest.
///
/// Reads the raw LE `f32` bytes with the guest memory helpers; registers past
/// the file's end are dropped (the writes are clamped to the file).
fn set_shader_constant_f(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [[f32; 4]],
    start_register: u32,
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let float_address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
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
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
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
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetPixelShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetPixelShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_ps_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetPixelShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetPixelShaderConstantF` (vtable slot 110).
pub fn handle_get_pixel_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetPixelShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetPixelShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_ps_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetPixelShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
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
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetVertexShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_f(
        engine,
        &mut d3d.d3d9_vs_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetVertexShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantF` (vtable slot 95).
pub fn handle_get_vertex_shader_constant_f(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShaderConstantF")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetVertexShaderConstantF")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_f(
        engine,
        &d3d.d3d9_vs_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetVertexShaderConstantF")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Common Set*ShaderConstantI body: copy `count` int4s from the guest into an
/// integer register file (clamped to the file end, like the float form).
fn set_shader_constant_i(
    engine: &mut dyn wie_cpu::CpuEngine,
    file: &mut [[i32; 4]],
    start_register: u32,
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let int_address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
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
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(16));
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
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(slot) = file.get_mut(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let bool_address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(4));
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
    data_ptr: u64,
    count: u32,
) -> Result<()> {
    if data_ptr == 0 {
        return Ok(());
    }
    for index in 0..count {
        let register = start_register
            .checked_add(index)
            .context("constant register index overflow")?;
        let Some(value) = file.get(usize::try_from(register).unwrap_or(usize::MAX)) else {
            break;
        };
        let address = data_ptr.wrapping_add(u64::from(index).wrapping_mul(4));
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
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShaderConstantI")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetVertexShaderConstantI")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_i(
        engine,
        &mut d3d.d3d9_vs_int_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetVertexShaderConstantI")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantI` (vtable slot 97).
pub fn handle_get_vertex_shader_constant_i(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShaderConstantI")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetVertexShaderConstantI")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_i(
        engine,
        &d3d.d3d9_vs_int_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetVertexShaderConstantI")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
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
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for SetVertexShaderConstantB")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetVertexShaderConstantB")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    set_shader_constant_b(
        engine,
        &mut d3d.d3d9_vs_bool_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from SetVertexShaderConstantB")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DDevice9::GetVertexShaderConstantB` (vtable slot 99).
pub fn handle_get_vertex_shader_constant_b(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for GetVertexShaderConstantB")?;
    let start_register = u32::try_from(engine.read_rdx()? & u64::from(u32::MAX))
        .context("start register does not fit u32")?;
    let data_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetVertexShaderConstantB")?;
    let count =
        u32::try_from(engine.read_r9()? & u64::from(u32::MAX)).context("count does not fit u32")?;

    let d3d = state.d3d9();
    get_shader_constant_b(
        engine,
        &d3d.d3d9_vs_bool_constants,
        start_register,
        data_ptr,
        count,
    )?;

    let return_address = engine
        .return_from_win64_api(D3D_OK)
        .context("failed to return from GetVertexShaderConstantB")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: D3D_OK,
    })
}

/// Handles `IDirect3DPixelShader9::Release` (vtable slot 2).
pub fn handle_pixel_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DPixelShader9::Release")?;

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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DPixelShader9::Release")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `IDirect3DVertexShader9::Release` (vtable slot 2).
pub fn handle_vertex_shader_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let this_pointer = engine
        .read_rcx()
        .context("failed to read RCX for IDirect3DVertexShader9::Release")?;

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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IDirect3DVertexShader9::Release")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
