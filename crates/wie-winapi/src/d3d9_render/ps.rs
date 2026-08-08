//! The PS 2.0 pixel-shader interpreter.

use super::flow::{BlockState, FlowMap, FlowState, compare};
use super::operand::{
    comp, comp_opt, read_operand, read_operand_i32, read_three, read_two, write_operand,
};
use super::sample::{TextureStage, sample_texture};
use crate::d3d9_shader::{
    Operand, PS_CONST_COUNT, PS_INPUT_COUNT, PS_SAMPLER_COUNT, PS_TEMP_COUNT, PsInstruction, PsOp,
    RegType,
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
    /// Loop counters for the ps_2_a/b `rep`/`loop` block forms.
    loop_regs: [i32; 4],
    /// `p0` — predicate register (`setp` / predication / `breakp`).
    pred: [f32; 4],
}
/// The ps_2_0 register-file mapping: which `RegType` reads/writes which
/// storage. The operand readers/writers live in [`super::operand`] and are
/// generic over this trait; this impl is the pixel stage's only operand code.
impl super::operand::ShaderRegisters for PsRegisters {
    fn fetch(&self, reg_type: RegType, index: usize) -> [f32; 4] {
        match reg_type {
            RegType::Temp => self.temp.get(index).copied().unwrap_or([0.0; 4]),
            RegType::Const => self.constants.get(index).copied().unwrap_or([0.0; 4]),
            RegType::Input => self.input.get(index).copied().unwrap_or([0.0; 4]),
            RegType::Texture => self.texcoord.get(index).copied().unwrap_or([0.0; 4]),
            // The ps_2_x loop/rep counters read int→float; the predicate reads
            // its stored float value (ps_2_a/b flow-control forms).
            RegType::Loop => {
                let ints = self.loop_regs;
                [
                    ints.first().copied().unwrap_or(0) as f32,
                    ints.get(1).copied().unwrap_or(0) as f32,
                    ints.get(2).copied().unwrap_or(0) as f32,
                    ints.get(3).copied().unwrap_or(0) as f32,
                ]
            }
            RegType::Predicate => self.pred,
            // ColorOut / Sampler / Other as a source is not valid ps_2_0.
            _ => [0.0; 4],
        }
    }

    fn fetch_i32(&self, _index: usize) -> [i32; 4] {
        // The ps_2_x `iN` file is one flat 4-wide counter array; the caller
        // applies the operand's swizzle.
        self.loop_regs
    }

    fn effective_index(&self, op: &Operand) -> usize {
        // ps_2_0 has no relative addressing.
        usize::from(op.reg_num)
    }

    fn slot(&mut self, reg_type: RegType, index: usize) -> Option<&mut [f32; 4]> {
        match reg_type {
            RegType::Temp => self.temp.get_mut(index),
            // `oDepth` writes are stored nowhere — the fragment depth is still
            // the interpolated z (documented; deferred with the vertex stage).
            // `p0` accepts `setp` results.
            RegType::ColorOut => Some(&mut self.output),
            RegType::Predicate => Some(&mut self.pred),
            _ => None,
        }
    }
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
/// `rsq(x<0) = 1/sqrt(|x|)`, `log2(0) = -inf`, `log2(x<0) = NaN`. The
/// ps_2_a/b additions run too: `texldp` divides the uv by the coordinate's w
/// before sampling, `texldb` passes the w as a mip bias (a no-op on the
/// single-level point sampler, documented), `dp2add`, and the block flow
/// control (`if`/`ifc`/`else`/`endif`, `rep`/`endrep`, `break`/`breakc`,
/// `loop`/`endloop`, `setp` + predication).
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
        loop_regs: [0; 4],
        pred: [0.0; 4],
    };
    let flow = FlowMap::build(program.instructions);
    let mut state = FlowState::default();
    let mut pc: usize = 0;

    while let Some(instr) = program.instructions.get(pc) {
        state.steps = state.steps.saturating_add(1);
        if state.steps > super::flow::MAX_SHADER_STEPS {
            break;
        }
        // Predication: the instruction runs only while p0.x != 0.
        if instr.predicated && comp(regs.pred, 0) == 0.0 {
            pc = pc.saturating_add(1);
            continue;
        }
        // `texkill` discards the fragment; every other arm falls through or
        // jumps (Some(target) from a flow-control arm).
        let mut discard = false;
        let jump = match instr.op {
            PsOp::End => break,
            PsOp::Nop | PsOp::Def | PsOp::Dcl | PsOp::DefB | PsOp::DefI | PsOp::Label => None,
            PsOp::Mov => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let value = read_operand(&regs, src);
                    write_operand(&mut regs, dst, value);
                }
                None
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
                None
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
                None
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
                None
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
                None
            }
            PsOp::Rcp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0);
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
                None
            }
            PsOp::Rsq => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let r = 1.0 / comp(read_operand(&regs, src), 0).abs().sqrt();
                    write_operand(&mut regs, dst, [r, r, r, r]);
                }
                None
            }
            PsOp::Dp3 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
                None
            }
            PsOp::Dp4 => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
                    write_operand(&mut regs, dst, [d, d, d, d]);
                }
                None
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
                None
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
                None
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
                None
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
                None
            }
            PsOp::Exp => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [2.0_f32.powf(sx), sy, sz, sw]);
                }
                None
            }
            PsOp::Log => {
                if let Some(dst) = &instr.dst
                    && let Some(src) = instr.srcs.first()
                {
                    let [sx, sy, sz, sw] = read_operand(&regs, src);
                    write_operand(&mut regs, dst, [sx.log2(), sy, sz, sw]);
                }
                None
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
                None
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
                None
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
                None
            }
            PsOp::Dp2Add => {
                if let Some(dst) = &instr.dst {
                    let [a, b, c] = read_three(&regs, &instr.srcs);
                    let s = a[0] * b[0] + a[1] * b[1] + comp(c, 0);
                    write_operand(&mut regs, dst, [s, s, s, s]);
                }
                None
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
                None
            }
            PsOp::TexLdP => {
                // texldp (project): sample at (tN.x / tN.w, tN.y / tN.w).
                if let Some(dst) = &instr.dst
                    && let Some(uv_src) = instr.srcs.first()
                {
                    let [u, v, _, w] = read_operand(&regs, uv_src);
                    let texel = instr
                        .srcs
                        .get(1)
                        .and_then(|sampler| program.samplers.get(usize::from(sampler.reg_num)))
                        .and_then(|stage| *stage)
                        .map_or(0, |stage| sample_texture(stage, u / w, v / w));
                    write_operand(&mut regs, dst, texel_to_float4(texel));
                }
                None
            }
            PsOp::TexLdB => {
                // texldb (bias): the w component biases the mip selection —
                // a no-op on the single-level point sampler (documented).
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
                None
            }
            PsOp::TexKill => {
                if let Some(src) = instr.srcs.first() {
                    let [x, y, z, w] = read_operand(&regs, src);
                    if x < 0.0 || y < 0.0 || z < 0.0 || w < 0.0 {
                        discard = true;
                    }
                }
                None
            }
            // ── ps_2_a/b flow control (the shared block engine) ─────────
            PsOp::If => {
                let cond = instr
                    .dst
                    .as_ref()
                    .is_some_and(|op| comp(read_operand(&regs, op), 0) != 0.0);
                if cond { None } else { flow.branch_target(pc) }
            }
            PsOp::Ifc => {
                let [a, b] = read_two(&regs, &instr.srcs);
                if compare(comp(a, 0), comp(b, 0), instr.control) {
                    None
                } else {
                    flow.branch_target(pc)
                }
            }
            PsOp::Else => flow.else_target(pc),
            PsOp::EndIf => None,
            PsOp::Loop => {
                if let (Some(dst), Some(src)) = (&instr.dst, instr.srcs.first())
                    && let Some(end) = flow.loop_end(pc)
                {
                    let spec = read_operand_i32(&regs, src);
                    let a_l = comp_opt(spec, 0);
                    let a_u = comp_opt(spec, 1).max(1);
                    let a_d = comp_opt(spec, 2);
                    regs.loop_regs[0] = a_l;
                    state.blocks.push(BlockState::Loop {
                        reg: dst.reg_num,
                        value: a_l,
                        remaining: a_u,
                        step: a_d,
                        start: pc,
                        end,
                    });
                }
                None
            }
            PsOp::EndLoop => {
                let Some(BlockState::Loop {
                    reg,
                    value,
                    remaining,
                    step,
                    start,
                    end: _,
                }) = state.blocks.last_mut()
                else {
                    return None;
                };
                *value = value.saturating_add(*step);
                *remaining = remaining.saturating_sub(1);
                regs.loop_regs[usize::from(*reg)] = *value;
                if *remaining > 0 {
                    Some(start.saturating_add(1))
                } else {
                    state.blocks.pop();
                    None
                }
            }
            PsOp::Rep => {
                if let (Some(dst), Some(src)) = (&instr.dst, instr.srcs.first())
                    && let Some(end) = flow.rep_end(pc)
                {
                    let count = comp_opt(read_operand_i32(&regs, src), 0).max(1);
                    regs.loop_regs[usize::from(dst.reg_num)] = count;
                    state.blocks.push(BlockState::Rep {
                        remaining: count,
                        start: pc,
                        end,
                    });
                }
                None
            }
            PsOp::EndRep => {
                let Some(BlockState::Rep {
                    remaining,
                    start,
                    end: _,
                }) = state.blocks.last_mut()
                else {
                    return None;
                };
                *remaining = remaining.saturating_sub(1);
                if *remaining > 0 {
                    Some(start.saturating_add(1))
                } else {
                    state.blocks.pop();
                    None
                }
            }
            PsOp::Break => break_from(&mut state.blocks),
            PsOp::BreakC => {
                let [a, b] = read_two(&regs, &instr.srcs);
                if compare(comp(a, 0), comp(b, 0), instr.control) {
                    break_from(&mut state.blocks)
                } else {
                    None
                }
            }
            PsOp::BreakP => {
                let pred = instr
                    .srcs
                    .first()
                    .map_or(0.0, |op| comp(read_operand(&regs, op), 0));
                if pred != 0.0 {
                    break_from(&mut state.blocks)
                } else {
                    None
                }
            }
            PsOp::Call => {
                let label = instr
                    .srcs
                    .first()
                    .or(instr.dst.as_ref())
                    .map_or(0, |op| op.reg_num);
                if state.call_stack.len() >= super::flow::CALL_DEPTH_LIMIT {
                    None
                } else {
                    state.call_stack.push(pc.saturating_add(1));
                    flow.label(label)
                }
            }
            PsOp::CallNz => {
                let should_call = instr
                    .srcs
                    .get(1)
                    .is_some_and(|op| comp(read_operand(&regs, op), 0) != 0.0);
                if should_call {
                    let label = instr
                        .srcs
                        .first()
                        .or(instr.dst.as_ref())
                        .map_or(0, |op| op.reg_num);
                    if state.call_stack.len() >= super::flow::CALL_DEPTH_LIMIT {
                        None
                    } else {
                        state.call_stack.push(pc.saturating_add(1));
                        flow.label(label)
                    }
                } else {
                    None
                }
            }
            PsOp::Ret => state.call_stack.pop(),
            PsOp::Setp => {
                if let Some(dst) = &instr.dst {
                    let [a, b] = read_two(&regs, &instr.srcs);
                    let result = [
                        if compare(comp(a, 0), comp(b, 0), instr.control) {
                            1.0
                        } else {
                            0.0
                        },
                        if compare(comp(a, 1), comp(b, 1), instr.control) {
                            1.0
                        } else {
                            0.0
                        },
                        if compare(comp(a, 2), comp(b, 2), instr.control) {
                            1.0
                        } else {
                            0.0
                        },
                        if compare(comp(a, 3), comp(b, 3), instr.control) {
                            1.0
                        } else {
                            0.0
                        },
                    ];
                    write_operand(&mut regs, dst, result);
                }
                None
            }
            // The vs-only arithmetic ops (mNxM, dst, lit, pow, crs, sgn, abs,
            // nrm, sincos, mova) cannot appear in valid ps_2_x bytecode —
            // they parse but never execute here.
            PsOp::M4x4
            | PsOp::M4x3
            | PsOp::M3x4
            | PsOp::M3x3
            | PsOp::M3x2
            | PsOp::Dst
            | PsOp::Lit
            | PsOp::Pow
            | PsOp::Crs
            | PsOp::Sgn
            | PsOp::Abs
            | PsOp::Nrm
            | PsOp::SinCos
            | PsOp::Mova => None,
            PsOp::Unsupported(_) => return None, // unreachable: Create rejects these
        };
        if discard {
            return None;
        }
        match jump {
            Some(target) => pc = target,
            None => pc = pc.saturating_add(1),
        }
    }
    Some(regs.output)
}
/// Pop the innermost `loop`/`rep` block and return its break target.
fn break_from(blocks: &mut Vec<BlockState>) -> Option<usize> {
    blocks.pop().map(|block| match block {
        BlockState::Loop { end, .. } | BlockState::Rep { end, .. } => end.saturating_add(1),
    })
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
