//! The PS 2.0 pixel-shader interpreter.

use super::sample::{TextureStage, sample_texture};
use crate::d3d9_shader::{
    D3DSPDM_SATURATE, D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG, D3DSPSM_COMP,
    D3DSPSM_NEG, D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2, D3DSPSM_X2NEG, Operand, PS_CONST_COUNT,
    PS_INPUT_COUNT, PS_SAMPLER_COUNT, PS_TEMP_COUNT, PsInstruction, PsOp, RegType,
};

// ── PS 2.0 interpreter (fragment stage) ────────────────────────────────

/// Executable pixel-shader program for one draw: the tokenized instructions
/// plus the constant and sampler state resolved at the draw boundary.
///
/// `constants` is copied per draw (32 float4s); `instructions` and `samplers`
/// borrow the bound shader record and texture records, which outlive the
/// rasterization call.
#[derive(Debug)]
pub struct PsProgram<'a> {
    /// Tokenized instructions (borrowed from the bound shader record).
    pub instructions: &'a [PsInstruction],
    /// Constant registers `c0..c31` (`SetPixelShaderConstantF` + `def`).
    pub constants: [[f32; 4]; PS_CONST_COUNT],
    /// Sampler registers `s0..s3` → the bound texture stage (None = unbound).
    pub samplers: [Option<&'a TextureStage<'a>>; PS_SAMPLER_COUNT],
}
/// Per-fragment interpolated inputs to the pixel shader.
#[derive(Debug, Clone, Copy)]
pub struct PsFragmentInput {
    /// `v0` — interpolated diffuse color (0..1 per channel, RGBA order).
    pub v0: [f32; 4],
    /// `v1` — specular color (unbound in the current FVF pipeline → 0).
    pub v1: [f32; 4],
    /// `t0` — interpolated texture-coordinate set 0 (`u, v, 0, 1`).
    pub t0: [f32; 4],
}
/// The ps_2_0 per-fragment register file.
struct PsRegisters {
    temp: [[f32; 4]; PS_TEMP_COUNT],
    constants: [[f32; 4]; PS_CONST_COUNT],
    input: [[f32; 4]; PS_INPUT_COUNT],
    /// `t0..t3` (only `t0` is interpolated; the rest read `(0,0,0,1)`).
    texcoord: [[f32; 4]; 4],
    /// `oC0` (the fragment color that feeds the blend stage).
    output: [f32; 4],
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
/// unmodeled and read as identity — they do not occur in the ps_2_0 subset
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
#[must_use]
fn read_operand(regs: &PsRegisters, op: &Operand) -> [f32; 4] {
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
        RegType::Texture => regs
            .texcoord
            .get(usize::from(op.reg_num))
            .copied()
            .unwrap_or([0.0; 4]),
        // ColorOut / Sampler / Other as a source is not valid ps_2_0.
        _ => [0.0; 4],
    };
    apply_swizzle(apply_src_mod(base, op.src_mod), op.swizzle)
}
/// Write an operand's value into the register file (write mask + saturate).
///
/// `_sat` clamps the written components to `[0, 1]`; `_pp` (partial
/// precision) is ignored (full f32 precision, documented). `oDepth` writes
/// are stored nowhere — the fragment depth is still the interpolated
/// z (documented; deferred with the vertex stage).
fn write_operand(regs: &mut PsRegisters, op: &Operand, value: [f32; 4]) {
    let mut result = value;
    if op.dst_mod == D3DSPDM_SATURATE {
        for channel in &mut result {
            *channel = channel.clamp(0.0, 1.0);
        }
    }
    let target = match op.reg_type {
        RegType::Temp => regs.temp.get_mut(usize::from(op.reg_num)),
        RegType::ColorOut => Some(&mut regs.output),
        _ => None, // oDepth etc. not applied
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
/// Convert a `0xAARRGGBB` texel to the shader's `[r, g, b, a]` 0..1 float4.
#[must_use]
fn texel_to_float4(texel: u32) -> [f32; 4] {
    [
        f32::from(u8::try_from((texel >> 16) & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from((texel >> 8) & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from(texel & 0xFF).unwrap_or(0)) / 255.0,
        f32::from(u8::try_from((texel >> 24) & 0xFF).unwrap_or(0)) / 255.0,
    ]
}
/// Convert a `0xAARRGGBB` diffuse color to the shader's `[r, g, b, a]` float4.
#[must_use]
pub(super) fn color_to_float4(color: u32) -> [f32; 4] {
    texel_to_float4(color)
}
/// Execute a pixel shader for one fragment; `None` = `texkill` discarded it.
///
/// Semantics follow the ps_2_0 spec (op operand order: `slt dst, a, b` is
/// `(a < b)`, `sge` is `(a >= b)`, `lrp dst, a, b, c` is `a*b + (1-a)*c`,
/// `cmp dst, a, b, c` is `(a >= 0) ? b : c`; `rcp`/`rsq`/`dp3`/`dp4` replicate
/// their scalar result to all four channels; `exp`/`log` compute only the x
/// channel and copy yzw). Domain edges use plain IEEE f32 arithmetic, which
/// matches the hardware contract: `rcp(0) = +inf`, `rsq(0) = +inf`,
/// `rsq(x<0) = 1/sqrt(|x|)`, `log2(0) = -inf`, `log2(x<0) = NaN`.
#[must_use]
pub fn run_pixel_shader(program: &PsProgram<'_>, input: &PsFragmentInput) -> Option<[f32; 4]> {
    let mut regs = PsRegisters {
        temp: [[0.0; 4]; PS_TEMP_COUNT],
        constants: program.constants,
        input: [input.v0, input.v1],
        texcoord: [
            input.t0,
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        output: [0.0; 4],
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
            PsOp::Tex => {
                // texld rD, tN, sM — sample sampler sM at (tN.x, tN.y).
                if let Some(dst) = &instr.dst
                    && let Some(uv_src) = instr.srcs.first()
                {
                    let [u, v, _, _] = read_operand(&regs, uv_src);
                    let texel = instr
                        .srcs
                        .get(1)
                        .and_then(|sampler| program.samplers.get(usize::from(sampler.reg_num)))
                        .and_then(|stage| *stage)
                        .map_or(0, |stage| sample_texture(stage, u, v));
                    write_operand(&mut regs, dst, texel_to_float4(texel));
                }
            }
            PsOp::TexKill => {
                if let Some(src) = instr.srcs.first() {
                    let [x, y, z, w] = read_operand(&regs, src);
                    if x < 0.0 || y < 0.0 || z < 0.0 || w < 0.0 {
                        return None;
                    }
                }
            }
            PsOp::Unsupported(_) => return None, // unreachable: Create rejects these
        }
    }
    Some(regs.output)
}
/// Read exactly two source operands; short source lists read as zero.
#[must_use]
fn read_two(regs: &PsRegisters, srcs: &[Operand]) -> [[f32; 4]; 2] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b]
}
/// Read exactly three source operands; short source lists read as zero.
#[must_use]
fn read_three(regs: &PsRegisters, srcs: &[Operand]) -> [[f32; 4]; 3] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    let c = srcs.get(2).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b, c]
}
/// Convert the shader's `oC0` float4 to an `0RGB` backbuffer color
/// (`round(v * 255)` per channel, clamped to 0..255).
#[must_use]
pub fn pixel_shader_color_to_0rgb(oc0: [f32; 4]) -> u32 {
    let channel = |v: f32| {
        let rounded = (v * 255.0).round().clamp(0.0, 255.0);
        rounded as u32
    };
    (channel(comp(oc0, 0)) << 16) | (channel(comp(oc0, 1)) << 8) | channel(comp(oc0, 2))
}
/// Convert the shader's `oC0` alpha (0..1) to the blend-stage alpha byte.
#[must_use]
pub fn pixel_shader_alpha_to_u8(oc0: [f32; 4]) -> u8 {
    let a = (comp(oc0, 3) * 255.0).round().clamp(0.0, 255.0);
    a as u8
}
