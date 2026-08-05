//! The VS 2.0 vertex-shader interpreter (vertex stage).
//!
//! Mirrors the PS 2.0 interpreter's architecture (`d3d9_render/ps.rs`): the
//! tokenized instruction list is the shared `ParsedShader` (the tokenizer in
//! `d3d9_shader.rs` serves both stages), executed once per vertex against the
//! FVF-decoded input registers. The output is `oPos` (clip space — the
//! caller applies the viewport transform + w-divide) plus `oD0` / `oT0` for
//! the diffuse color and texture-coordinate set 0 the fragment stage consumes.
//!
//! The executable op set is the PS interpreter's arithmetic core mirrored
//! (`NOP`/`MOV`/`ADD`/`SUB`/`MAD`/`MUL`/`DP3`/`DP4`/`MIN`/`MAX`/`SLT`/`SGE`/
//! `EXP`/`LOG`/`LRP`/`FRC`/`CMP`/`RCP`/`RSQ` + `DEF`/`DCL`/`END`); `TEX` /
//! `TEXKILL` do not exist in vertex shaders. The advanced ops (`m4x4`, `dst`,
//! `lit`, `pow`, `crs`, `sgn`, `abs`, `nrm`, `sincos`) and ALL flow control
//! (`mova`, `if`/`else`/`endif`, `loop`/`endloop`, `rep`, `call`/`ret`, …) are
//! L5: they parse as `PsOp::Unsupported` and `CreateVertexShader` rejects the
//! shader via `ParsedShader::is_fully_executable` until then.
//!
//! Register files: `v0..v15` (FVF stream inputs), `r0..r11` (temporaries),
//! `c0..c255` (constants), `a0` (address), `b0..b15` (boolean constants),
//! `i0..i3` (loop registers), `oPos`/`oFog`/`oPts` (rasterizer outputs),
//! `oD0..oD1` (color outputs), `oT0..oT7` (texcoord outputs). `a0`, `bN` and
//! `iN` are stored but always zero in L1 — the ops that write them (`mova`,
//! `defb`, `defi`, `loop`) are L5; reads of a zeroed address register are the
//! vs_2_0 default before any `mova`.

use super::ps::color_to_float4;
use super::vertex::{FvfLayout, GuestVertex};
use crate::d3d9_shader::{
    D3DSPDM_SATURATE, D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG, D3DSPSM_COMP,
    D3DSPSM_NEG, D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2, D3DSPSM_X2NEG, Operand, PsInstruction,
    PsOp, RegType, VS_CONST_COUNT,
};

// ── vs_2_0 register-file sizes (the interpreter's contract) ─────────────

/// `v0..v15` — vertex input registers (FVF stream semantics).
const VS_INPUT_COUNT: usize = 16;
/// `r0..r11` — temporary registers.
const VS_TEMP_COUNT: usize = 12;
/// `b0..b15` — boolean constant registers.
const VS_BOOL_CONST_COUNT: usize = 16;
/// `i0..i3` — integer loop registers.
const VS_LOOP_COUNT: usize = 4;
/// `oPos`/`oFog`/`oPts`.
const VS_RASTOUT_COUNT: usize = 3;
/// `oD0`/`oD1`.
const VS_ATTROUT_COUNT: usize = 2;
/// `oT0..oT7`.
const VS_TEXCRDOUT_COUNT: usize = 8;

/// The FVF semantic register numbers the vs input mapping fills (the D3D9
/// `D3DVSDE_*` usage slots): position 0, texcoord0 5, diffuse 6.
const VS_INPUT_POSITION: usize = 0;
const VS_INPUT_TEXCOORD0: usize = 5;
const VS_INPUT_DIFFUSE: usize = 6;

/// Executable vertex-shader program for one draw: the tokenized instructions
/// plus the constant registers resolved at the draw boundary.
///
/// `instructions` borrow the bound shader record (which outlives the
/// rasterization call); `constants` is copied per draw (256 float4s).
#[derive(Debug)]
pub struct VsProgram<'a> {
    /// Tokenized instructions (borrowed from the bound shader record).
    pub instructions: &'a [PsInstruction],
    /// Constant registers `c0..c255` (`SetVertexShaderConstantF` + `def`).
    pub constants: [[f32; 4]; VS_CONST_COUNT],
}

/// The vs_2_0 per-vertex input register file, decoded from the FVF stream.
#[derive(Debug, Clone, Copy)]
pub struct VsVertexInput {
    /// `v0..v15`; registers the FVF does not supply read `(0,0,0,0)`.
    pub v: [[f32; 4]; VS_INPUT_COUNT],
}

/// The vertex stage's output for one vertex.
#[derive(Debug, Clone, Copy)]
pub struct VsOutput {
    /// `oPos` — the clip-space position (the caller applies the viewport
    /// transform + w-divide). `(0,0,0,0)` when the shader never wrote it.
    pub pos: [f32; 4],
    /// `oD0` converted to `0xAARRGGBB` (black when the shader never wrote it).
    pub color: u32,
    /// `oT0.x` — texture-coordinate set 0 U.
    pub u: f32,
    /// `oT0.y` — texture-coordinate set 0 V.
    pub v: f32,
}

/// The vs_2_0 per-vertex register file.
struct VsRegisters {
    temp: [[f32; 4]; VS_TEMP_COUNT],
    constants: [[f32; 4]; VS_CONST_COUNT],
    input: [[f32; 4]; VS_INPUT_COUNT],
    /// `a0` — address register (zero until `mova`, which is L5).
    addr: [f32; 4],
    /// `b0..b15` — boolean constants (zero until `defb`, which is L5).
    const_bool: [[f32; 4]; VS_BOOL_CONST_COUNT],
    /// `i0..i3` — loop registers (zero until `defi`/`loop`, which are L5).
    loop_regs: [[f32; 4]; VS_LOOP_COUNT],
    /// `oPos`(0) / `oFog`(1) / `oPts`(2). `oFog`/`oPts` are stored, unused in
    /// L1 (no fog / point rendering).
    rast_out: [[f32; 4]; VS_RASTOUT_COUNT],
    /// `oD0`(0) / `oD1`(1).
    attr_out: [[f32; 4]; VS_ATTROUT_COUNT],
    /// `oT0..oT7`.
    texcrd_out: [[f32; 4]; VS_TEXCRDOUT_COUNT],
}

/// Read the `i`-th component without indexing syntax (repo lint).
#[inline]
#[must_use]
fn comp(value: [f32; 4], i: usize) -> f32 {
    value.get(i).copied().unwrap_or(0.0)
}

/// Apply a source modifier (`D3DSPSM_*`) to a register value.
///
/// `DZ`/`DW` (texcoord-depth modifiers) and `NOT` (boolean registers) are
/// unmodeled and read as identity — they do not occur in the vs_2_0 subset
/// the interpreter executes.
#[must_use]
fn apply_src_mod(value: [f32; 4], src_mod: u8) -> [f32; 4] {
    let [x, y, z, w] = value;
    match src_mod {
        D3DSPSM_NEG => [-x, -y, -z, -w],
        D3DSPSM_BIAS => [x - 0.5, y - 0.5, z - 0.5, w - 0.5],
        D3DSPSM_BIASNEG => [0.5 - x, 0.5 - y, 0.5 - z, 0.5 - w],
        D3DSPSM_SIGN => [
            if x >= 0.0 { 1.0 } else { -1.0 },
            if y >= 0.0 { 1.0 } else { -1.0 },
            if z >= 0.0 { 1.0 } else { -1.0 },
            if w >= 0.0 { 1.0 } else { -1.0 },
        ],
        D3DSPSM_SIGNNEG => [
            if x >= 0.0 { -1.0 } else { 1.0 },
            if y >= 0.0 { -1.0 } else { 1.0 },
            if z >= 0.0 { -1.0 } else { 1.0 },
            if w >= 0.0 { -1.0 } else { 1.0 },
        ],
        D3DSPSM_COMP => [1.0 - x, 1.0 - y, 1.0 - z, 1.0 - w],
        D3DSPSM_X2 => [2.0 * x, 2.0 * y, 2.0 * z, 2.0 * w],
        D3DSPSM_X2NEG => [-2.0 * x, -2.0 * y, -2.0 * z, -2.0 * w],
        D3DSPSM_ABS => [x.abs(), y.abs(), z.abs(), w.abs()],
        D3DSPSM_ABSNEG => [-x.abs(), -y.abs(), -z.abs(), -w.abs()],
        // NONE and any unmodeled modifier pass the value through.
        _ => [x, y, z, w],
    }
}

/// Reorder a register value by the operand's per-component swizzle.
#[must_use]
fn apply_swizzle(value: [f32; 4], swizzle: [u8; 4]) -> [f32; 4] {
    [
        comp(value, usize::from(swizzle[0])),
        comp(value, usize::from(swizzle[1])),
        comp(value, usize::from(swizzle[2])),
        comp(value, usize::from(swizzle[3])),
    ]
}

/// Read a source operand: register fetch → source modifier → swizzle.
///
/// `RegType::Texture` reads the `a0` address register here (the register-type
/// value 3 is shared with the pixel stage's `tN`; the vertex stage decides by
/// context — see [`RegType`]).
#[must_use]
fn read_operand(regs: &VsRegisters, op: &Operand) -> [f32; 4] {
    let base = match op.reg_type {
        RegType::Temp => regs
            .temp
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Const => regs
            .constants
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Input => regs
            .input
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Texture => regs.addr,
        RegType::RastOut => regs
            .rast_out
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::AttrOut => regs
            .attr_out
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::TexcrdOut => regs
            .texcrd_out
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::ConstBool => regs
            .const_bool
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        RegType::Loop => regs
            .loop_regs
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        // ColorOut / DepthOut / Sampler / Other as a source is not valid vs_2_0.
        _ => [0.0; 4],
    };
    apply_swizzle(apply_src_mod(base, op.src_mod), op.swizzle)
}

/// Write an operand's value into the register file (write mask + saturate).
///
/// `_sat` clamps the written components to `[0, 1]`; `_pp` (partial
/// precision) is ignored (full f32 precision, documented). Writes to the
/// constant/bool/loop/input files are dropped (not valid vs_2_0
/// destinations). The `a0` register accepts writes (`mova` is L5, so in L1
/// nothing writes it).
fn write_operand(regs: &mut VsRegisters, op: &Operand, value: [f32; 4]) {
    let mut result = value;
    if op.dst_mod == D3DSPDM_SATURATE {
        for channel in &mut result {
            *channel = channel.clamp(0.0, 1.0);
        }
    }
    let target = match op.reg_type {
        RegType::Temp => regs.temp.get_mut(usize::from(op.reg_num)),
        RegType::RastOut => regs.rast_out.get_mut(usize::from(op.reg_num)),
        RegType::AttrOut => regs.attr_out.get_mut(usize::from(op.reg_num)),
        RegType::TexcrdOut => regs.texcrd_out.get_mut(usize::from(op.reg_num)),
        RegType::Texture => Some(&mut regs.addr),
        _ => None,
    };
    let Some(target) = target else { return };
    let [x, y, z, w] = result;
    let [ox, oy, oz, ow] = *target;
    *target = [
        if op.write_mask & 0x1 != 0 { x } else { ox },
        if op.write_mask & 0x2 != 0 { y } else { oy },
        if op.write_mask & 0x4 != 0 { z } else { oz },
        if op.write_mask & 0x8 != 0 { w } else { ow },
    ];
}

/// Convert a float4 color to `0xAARRGGBB` (`round(v * 255)` per channel,
/// clamped to 0..255). The shader's `oD0` is `[r, g, b, a]`.
#[must_use]
fn float4_to_0argb(color: [f32; 4]) -> u32 {
    let channel = |v: f32| {
        let rounded = (v * 255.0).round().clamp(0.0, 255.0);
        rounded as u32
    };
    (channel(comp(color, 3)) << 24)
        | (channel(comp(color, 0)) << 16)
        | (channel(comp(color, 1)) << 8)
        | channel(comp(color, 2))
}

/// Decode one FVF vertex into the vs input registers `v0..v15`.
///
/// The mapping covers the FVF subset the pipeline decodes: `v0` = position,
/// `v5` = texcoord set 0 (when `D3DFVF_TEX1` is present), `v6` = diffuse
/// color (when `D3DFVF_DIFFUSE`). Normal/specular would land in `v3`/`v7`
/// per the D3D9 usage slots — deferred (the `GuestVertex` decoder does not
/// carry them); the registers read zero until then.
#[must_use]
pub fn vs_input_from_vertex(v: &GuestVertex, layout: &FvfLayout) -> VsVertexInput {
    let mut regs = [[0.0_f32; 4]; VS_INPUT_COUNT];
    if let Some(slot) = regs.get_mut(VS_INPUT_POSITION) {
        *slot = [v.x, v.y, v.z, v.w];
    }
    if layout.has_diffuse
        && let Some(slot) = regs.get_mut(VS_INPUT_DIFFUSE)
    {
        *slot = color_to_float4(v.color);
    }
    if layout.tex_coords > 0
        && let Some(slot) = regs.get_mut(VS_INPUT_TEXCOORD0)
    {
        *slot = [v.u, v.v, 0.0, 1.0];
    }
    VsVertexInput { v: regs }
}

/// Execute a vertex shader for one vertex; returns the register-file outputs.
///
/// Semantics mirror the PS interpreter op for op (`slt dst, a, b` is
/// `(a < b)`, `lrp` is `a*b + (1-a)*c`, `cmp` is `(a >= 0) ? b : c`, …).
/// Domain edges use plain IEEE f32 arithmetic (`rcp(0) = +inf`, `rsq(x<0) =
/// 1/sqrt(|x|)`) — the same documented contract as the pixel stage.
#[must_use]
pub fn run_vertex_shader(program: &VsProgram<'_>, input: &VsVertexInput) -> VsOutput {
    let mut regs = VsRegisters {
        temp: [[0.0; 4]; VS_TEMP_COUNT],
        constants: program.constants,
        input: input.v,
        addr: [0.0; 4],
        const_bool: [[0.0; 4]; VS_BOOL_CONST_COUNT],
        loop_regs: [[0.0; 4]; VS_LOOP_COUNT],
        rast_out: [[0.0; 4]; VS_RASTOUT_COUNT],
        attr_out: [[0.0; 4]; VS_ATTROUT_COUNT],
        texcrd_out: [[0.0; 4]; VS_TEXCRDOUT_COUNT],
    };

    for instr in program.instructions {
        match instr.op {
            PsOp::End => break,
            PsOp::Nop | PsOp::Def | PsOp::Dcl => {}
            PsOp::Mov => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let value = read_operand(&regs, src);
                    write_operand(&mut regs, dst, value);
                }
            }
            PsOp::Add => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]],
                    );
                }
            }
            PsOp::Sub => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]],
                    );
                }
            }
            PsOp::Mad => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0] * b[0] + c[0],
                            a[1] * b[1] + c[1],
                            a[2] * b[2] + c[2],
                            a[3] * b[3] + c[3],
                        ],
                    );
                }
            }
            PsOp::Mul => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [a[0] * b[0], a[1] * b[1], a[2] * b[2], a[3] * b[3]],
                    );
                }
            }
            PsOp::Rcp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0);
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
            }
            PsOp::Rsq => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0).abs().sqrt();
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
            }
            PsOp::Dp3 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
            }
            PsOp::Dp4 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
            }
            PsOp::Min => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0].min(b[0]),
                            a[1].min(b[1]),
                            a[2].min(b[2]),
                            a[3].min(b[3]),
                        ],
                    );
                }
            }
            PsOp::Max => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0].max(b[0]),
                            a[1].max(b[1]),
                            a[2].max(b[2]),
                            a[3].max(b[3]),
                        ],
                    );
                }
            }
            PsOp::Slt => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] < b[0] { 1.0 } else { 0.0 },
                            if a[1] < b[1] { 1.0 } else { 0.0 },
                            if a[2] < b[2] { 1.0 } else { 0.0 },
                            if a[3] < b[3] { 1.0 } else { 0.0 },
                        ],
                    );
                }
            }
            PsOp::Sge => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] >= b[0] { 1.0 } else { 0.0 },
                            if a[1] >= b[1] { 1.0 } else { 0.0 },
                            if a[2] >= b[2] { 1.0 } else { 0.0 },
                            if a[3] >= b[3] { 1.0 } else { 0.0 },
                        ],
                    );
                }
            }
            PsOp::Exp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [2.0_f32.powf(sx), sy, sz, sw]);
                }
            }
            PsOp::Log => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [sx.log2(), sy, sz, sw]);
                }
            }
            PsOp::Lrp => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            a[0] * b[0] + (1.0 - a[0]) * c[0],
                            a[1] * b[1] + (1.0 - a[1]) * c[1],
                            a[2] * b[2] + (1.0 - a[2]) * c[2],
                            a[3] * b[3] + (1.0 - a[3]) * c[3],
                        ],
                    );
                }
            }
            PsOp::Frc => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [x, y, z, w] = read_operand(&regs, src);
                    write_operand(
                        &mut regs,
                        dst,
                        [x - x.floor(), y - y.floor(), z - z.floor(), w - w.floor()],
                    );
                }
            }
            PsOp::Cmp => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    write_operand(
                        &mut regs,
                        dst,
                        [
                            if a[0] >= 0.0 { b[0] } else { c[0] },
                            if a[1] >= 0.0 { b[1] } else { c[1] },
                            if a[2] >= 0.0 { b[2] } else { c[2] },
                            if a[3] >= 0.0 { b[3] } else { c[3] },
                        ],
                    );
                }
            }
            // texld / texkill have no vertex-shader form; Unsupported is
            // unreachable (Create rejects such shaders via the gate).
            PsOp::Tex | PsOp::TexKill | PsOp::Unsupported(_) => break,
        }
    }

    let pos = regs.rast_out.first().copied().unwrap_or([0.0; 4]);
    let o_d0 = regs.attr_out.first().copied().unwrap_or([0.0; 4]);
    let o_t0 = regs.texcrd_out.first().copied().unwrap_or([0.0; 4]);
    VsOutput {
        pos,
        color: float4_to_0argb(o_d0),
        u: comp(o_t0, 0),
        v: comp(o_t0, 1),
    }
}

/// Read exactly two source operands; short source lists read as zero.
#[must_use]
fn read_two(regs: &VsRegisters, srcs: &[Operand]) -> [[f32; 4]; 2] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b]
}

/// Read exactly three source operands; short source lists read as zero.
#[must_use]
fn read_three(regs: &VsRegisters, srcs: &[Operand]) -> [[f32; 4]; 3] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    let c = srcs.get(2).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b, c]
}
