//! The VS 2.0 vertex-shader interpreter (vertex stage).
//!
//! Mirrors the PS 2.0 interpreter's architecture (`d3d9_render/ps.rs`): the
//! tokenized instruction list is the shared `ParsedShader` (the tokenizer in
//! `d3d9_shader.rs` serves both stages), executed once per vertex against the
//! FVF-decoded input registers. The output is `oPos` (clip space — the
//! caller applies the viewport transform + w-divide) plus `oD0` / `oT0` for
//! the diffuse color and texture-coordinate set 0 the fragment stage consumes.
//!
//! The executable op set is the full vs_2_0 instruction set: the arithmetic
//! core (mirroring the PS interpreter) plus the L5 advanced ops (`m4x4`..
//! `m3x2` matrix multiplies, `dst`, `lit`, `pow`, `crs`, `sgn`, `abs`, `nrm`,
//! `sincos`, `dp2add`, `mova`) and ALL flow control (`if`/`ifc`/`else`/
//! `endif`, `loop`/`endloop`, `rep`/`endrep`, `break`/`breakc`/`breakp`,
//! `call`/`callnz`/`label`/`ret`, `setp` + predication). `TEX` / `TEXKILL`
//! do not exist in vertex shaders.
//!
//! Register files: `v0..v15` (FVF stream inputs), `r0..r11` (temporaries),
//! `c0..c255` (constants), `a0` (address — written by `mova`, drives relative
//! addressing `c[a0.x + n]`), `b0..b15` (boolean constants), `i0..i3`
//! (integer/loop registers), `p0` (predicate), `oPos`/`oFog`/`oPts`
//! (rasterizer outputs), `oD0..oD1` (color outputs), `oT0..oT7` (texcoord
//! outputs). `bN`/`iN` are initialized from the device's constant registers
//! (`SetVertexShaderConstantB/I` + `defb`/`defi` at Create time).
//!
//! Flow control runs through the shared machinery in [`super::flow`]: jump
//! targets are precomputed once per vertex ([`FlowMap::build`]) and the
//! runtime call/loop stacks live in a [`FlowState`]. The step budget bounds
//! a hostile shader's total work.

use super::flow::{BlockState, FlowMap, FlowState, compare};
use super::operand::{
    ShaderRegisters, apply_src_mod, apply_swizzle, comp, comp_opt, read_operand, read_operand_i32,
    read_three, read_two, write_operand,
};
use super::ps::color_to_float4;
use super::vertex::{FvfLayout, GuestVertex};
use crate::d3d9_shader::{
    Operand, PsInstruction, PsOp, RegType, VS_BOOL_CONST_COUNT, VS_CONST_COUNT, VS_INT_CONST_COUNT,
};

// ── vs_2_0 register-file sizes (the interpreter's contract) ─────────────

/// `v0..v15` — vertex input registers (FVF stream semantics).
const VS_INPUT_COUNT: usize = 16;
/// `r0..r11` — temporary registers.
const VS_TEMP_COUNT: usize = 12;
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
/// rasterization call); the three constant files are copied per draw
/// (`SetVertexShaderConstantF/I/B` + `def`/`defb`/`defi` from Create time).
#[derive(Debug)]
pub struct VsProgram<'a> {
    /// Tokenized instructions (borrowed from the bound shader record).
    pub instructions: &'a [PsInstruction],
    /// Constant registers `c0..c255` (`SetVertexShaderConstantF` + `def`).
    pub constants: [[f32; 4]; VS_CONST_COUNT],
    /// Integer constant registers `i0..i3` (`SetVertexShaderConstantI` +
    /// `defi`). `iN` also carries the live `loop`/`rep` counters.
    pub int_constants: [[i32; 4]; VS_INT_CONST_COUNT],
    /// Boolean constant registers `b0..b15` (`SetVertexShaderConstantB` +
    /// `defb`).
    pub bool_constants: [bool; VS_BOOL_CONST_COUNT],
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
    /// `a0` — address register (written by `mova`; the integer part drives
    /// relative addressing `c[a0.x + n]`).
    addr: [f32; 4],
    /// `b0..b15` — boolean constants.
    const_bool: [bool; VS_BOOL_CONST_COUNT],
    /// `i0..i3` — integer loop registers (the live `loop` counters live here).
    loop_regs: [[i32; 4]; VS_INT_CONST_COUNT],
    /// `p0` — predicate register (`setp` writes it; predicated instructions
    /// and `breakp` read `.x`).
    pred: [f32; 4],
    /// `oPos`(0) / `oFog`(1) / `oPts`(2). `oFog`/`oPts` are stored, unused
    /// (no fog / point rendering).
    rast_out: [[f32; 4]; VS_RASTOUT_COUNT],
    /// `oD0`(0) / `oD1`(1).
    attr_out: [[f32; 4]; VS_ATTROUT_COUNT],
    /// `oT0..oT7`.
    texcrd_out: [[f32; 4]; VS_TEXCRDOUT_COUNT],
}

/// The vs_2_0 register-file mapping: which `RegType` reads/writes which
/// storage. The operand readers/writers live in [`super::operand`] and are
/// generic over this trait; this impl is the vertex stage's only operand
/// code (its semantic additions: relative `c[a0.x + n]` addressing and the
/// int/bool constant files).
impl ShaderRegisters for VsRegisters {
    fn fetch(&self, reg_type: RegType, index: usize) -> [f32; 4] {
        match reg_type {
            RegType::Temp => self.temp.get(index).copied().unwrap_or([0.0; 4]),
            RegType::Const => self.constants.get(index).copied().unwrap_or([0.0; 4]),
            RegType::Input => self.input.get(index).copied().unwrap_or([0.0; 4]),
            // The register-type value 3 is shared with the pixel stage's `tN`;
            // the vertex stage reads the `a0` address register here.
            RegType::Texture => self.addr,
            RegType::RastOut => self.rast_out.get(index).copied().unwrap_or([0.0; 4]),
            RegType::AttrOut => self.attr_out.get(index).copied().unwrap_or([0.0; 4]),
            RegType::TexcrdOut => self.texcrd_out.get(index).copied().unwrap_or([0.0; 4]),
            // Boolean registers read 1.0/0.0 (D3D9's boolean→float semantics).
            RegType::ConstBool => {
                if self.const_bool.get(index).copied().unwrap_or(false) {
                    [1.0; 4]
                } else {
                    [0.0; 4]
                }
            }
            // Integer registers read as int→float (a `loop` counter of 2 reads
            // 2.0, matching the hardware's int→float source conversion).
            RegType::Loop => {
                let ints = self.loop_regs.get(index).copied().unwrap_or([0; 4]);
                [
                    ints[0] as f32,
                    ints[1] as f32,
                    ints[2] as f32,
                    ints[3] as f32,
                ]
            }
            RegType::Predicate => self.pred,
            // ColorOut / DepthOut / Sampler / Label / Other as a source is not
            // valid vs_2_0.
            _ => [0.0; 4],
        }
    }

    fn fetch_i32(&self, index: usize) -> [i32; 4] {
        self.loop_regs.get(index).copied().unwrap_or([0; 4])
    }

    fn effective_index(&self, op: &Operand) -> usize {
        let base = i64::from(op.reg_num);
        let offset = if op.relative {
            i64::from(comp(self.addr, 0).trunc() as i32)
        } else {
            0
        };
        usize::try_from(base + offset).unwrap_or(0)
    }

    fn slot(&mut self, reg_type: RegType, index: usize) -> Option<&mut [f32; 4]> {
        match reg_type {
            RegType::Temp => self.temp.get_mut(index),
            RegType::RastOut => self.rast_out.get_mut(index),
            RegType::AttrOut => self.attr_out.get_mut(index),
            RegType::TexcrdOut => self.texcrd_out.get_mut(index),
            // The `a0` register accepts writes (`mova`), and `p0` accepts
            // `setp` results.
            RegType::Texture => Some(&mut self.addr),
            RegType::Predicate => Some(&mut self.pred),
            // Writes to the constant/bool/loop/input files are dropped (not
            // valid vs_2_0 destinations).
            _ => None,
        }
    }
}

/// Read a matrix row: like [`read_operand`] but from `base + row_offset`
/// (the `mNxM` ops read `M` consecutive registers starting at the matrix
/// source).
#[must_use]
fn read_operand_row(regs: &VsRegisters, op: &Operand, row_offset: usize) -> [f32; 4] {
    let index = regs.effective_index(op).saturating_add(row_offset);
    let base = regs.fetch(op.reg_type, index);
    apply_swizzle(apply_src_mod(base, op.src_mod), op.swizzle)
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

/// Evaluate one `mNxM` matrix multiply (`row-vector × 4xN` semantics).
///
/// `vector_len` = the source vector's used components (the matrix's row
/// count); `cols` = the matrix's column count = the destination channels.
/// The matrix rows are `cols` consecutive registers starting at `matrix`,
/// each read with the matrix operand's swizzle/modifier: `dst[c] =
/// Σ_j src0[j]·M[c][j]`.
#[must_use]
fn matrix_multiply(
    regs: &VsRegisters,
    vector: [f32; 4],
    matrix: &Operand,
    vector_len: usize,
    cols: usize,
) -> [f32; 4] {
    let mut result = [0.0; 4];
    for col in 0..cols {
        let row = read_operand_row(regs, matrix, col);
        let mut dot = 0.0;
        for j in 0..vector_len {
            dot += comp(vector, j) * comp(row, j);
        }
        if let Some(slot) = result.get_mut(col) {
            *slot = dot;
        }
    }
    result
}

/// Execute one instruction at `pc`; `Some(target)` jumps the pc, `None`
/// falls through to the next instruction.
///
/// The flow-control arms mutate [`FlowState`] (the call/loop/rep stacks) and
/// return jump targets; the arithmetic arms return `None`. A missing jump
/// target (unmatched block or unknown label) also returns `None` — the caller
/// treats it as "end the shader".
#[allow(clippy::too_many_lines)]
fn execute_one(
    pc: usize,
    instr: &PsInstruction,
    regs: &mut VsRegisters,
    flow: &FlowMap,
    state: &mut FlowState,
) -> Option<usize> {
    match instr.op {
        PsOp::End | PsOp::Nop | PsOp::Def | PsOp::Dcl | PsOp::DefB | PsOp::DefI | PsOp::Label => {
            None
        }
        PsOp::Mov => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let value = read_operand(regs, src);
                write_operand(regs, dst, value);
            }
            None
        }
        PsOp::Add => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
                    dst,
                    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]],
                );
            }
            None
        }
        PsOp::Sub => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
                    dst,
                    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]],
                );
            }
            None
        }
        PsOp::Mad => {
            if let Some(dst) = &instr.dst {
                let [a, b, c] = read_three(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let r = 1.0 / comp(read_operand(regs, src), 0);
                write_operand(regs, dst, [r, r, r, r]);
            }
            None
        }
        PsOp::Rsq => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let r = 1.0 / comp(read_operand(regs, src), 0).abs().sqrt();
                write_operand(regs, dst, [r, r, r, r]);
            }
            None
        }
        PsOp::Dp3 => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
                write_operand(regs, dst, [d, d, d, d]);
            }
            None
        }
        PsOp::Dp4 => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
                write_operand(regs, dst, [d, d, d, d]);
            }
            None
        }
        PsOp::Min => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [sx, sy, sz, sw] = read_operand(regs, src);
                write_operand(regs, dst, [2.0_f32.powf(sx), sy, sz, sw]);
            }
            None
        }
        PsOp::Log => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let [sx, sy, sz, sw] = read_operand(regs, src);
                write_operand(regs, dst, [sx.log2(), sy, sz, sw]);
            }
            None
        }
        PsOp::Lrp => {
            if let Some(dst) = &instr.dst {
                let [a, b, c] = read_three(regs, &instr.srcs);
                write_operand(
                    regs,
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
                let [x, y, z, w] = read_operand(regs, src);
                write_operand(
                    regs,
                    dst,
                    [x - x.floor(), y - y.floor(), z - z.floor(), w - w.floor()],
                );
            }
            None
        }
        PsOp::Cmp => {
            if let Some(dst) = &instr.dst {
                let [a, b, c] = read_three(regs, &instr.srcs);
                write_operand(
                    regs,
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
        // ── L5 vs_2_0 advanced arithmetic ───────────────────────────────
        PsOp::M4x4 => matrix_mul_op(regs, instr, 4, 4),
        PsOp::M4x3 => matrix_mul_op(regs, instr, 4, 3),
        PsOp::M3x4 => matrix_mul_op(regs, instr, 3, 4),
        PsOp::M3x3 => matrix_mul_op(regs, instr, 3, 3),
        PsOp::M3x2 => matrix_mul_op(regs, instr, 3, 2),
        PsOp::Dst => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(regs, dst, [1.0, a[1] * b[1], a[2], b[3]]);
            }
            None
        }
        PsOp::Lit => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let [sx, sy, _, sw] = read_operand(regs, src);
                if sx > 0.0 {
                    write_operand(regs, dst, [1.0, sx, sy.powf(sw), 1.0]);
                } else {
                    write_operand(regs, dst, [1.0, 0.0, 0.0, 1.0]);
                }
            }
            None
        }
        PsOp::Pow => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                let p = a[0].powf(b[0]);
                write_operand(regs, dst, [p, p, p, p]);
            }
            None
        }
        PsOp::Crs => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
                write_operand(
                    regs,
                    dst,
                    [
                        a[1] * b[2] - a[2] * b[1],
                        a[2] * b[0] - a[0] * b[2],
                        a[0] * b[1] - a[1] * b[0],
                        0.0,
                    ],
                );
            }
            None
        }
        PsOp::Sgn => {
            if let Some(dst) = &instr.dst {
                // src1/src2 are the compiler-provided -1/+1 constants; the
                // sign of src0 selects between them (0.0 at exactly zero).
                let [a, neg, pos] = read_three(regs, &instr.srcs);
                let sign = |v: f32| {
                    if v > 0.0 {
                        comp(pos, 0)
                    } else if v < 0.0 {
                        comp(neg, 0)
                    } else {
                        0.0
                    }
                };
                write_operand(regs, dst, [sign(a[0]), sign(a[1]), sign(a[2]), sign(a[3])]);
            }
            None
        }
        PsOp::Abs => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let [x, y, z, w] = read_operand(regs, src);
                write_operand(regs, dst, [x.abs(), y.abs(), z.abs(), w.abs()]);
            }
            None
        }
        PsOp::Nrm => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                let [x, y, z, _] = read_operand(regs, src);
                let len = (x * x + y * y + z * z).sqrt();
                write_operand(regs, dst, [x / len, y / len, z / len, 0.0]);
            }
            None
        }
        PsOp::SinCos => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                // src1/src2 are required-but-unused operands in vs_2_0.
                let angle = comp(read_operand(regs, src), 0);
                write_operand(regs, dst, [angle.cos(), angle.sin(), 0.0, 0.0]);
            }
            None
        }
        PsOp::Dp2Add => {
            if let Some(dst) = &instr.dst {
                let [a, b, c] = read_three(regs, &instr.srcs);
                let s = a[0] * b[0] + a[1] * b[1] + comp(c, 0);
                write_operand(regs, dst, [s, s, s, s]);
            }
            None
        }
        PsOp::Mova => {
            if let Some(dst) = &instr.dst
                && let Some(src) = instr.srcs.first()
            {
                // The address register holds the truncated source (the
                // relative-addressing index); the integer part is what reads
                // back as a float source, matching the hardware.
                let [x, y, z, w] = read_operand(regs, src);
                write_operand(regs, dst, [x.trunc(), y.trunc(), z.trunc(), w.trunc()]);
            }
            None
        }
        // ── L5 flow control ─────────────────────────────────────────────
        PsOp::If => {
            let cond = instr
                .dst
                .as_ref()
                .is_some_and(|op| comp(read_operand(regs, op), 0) != 0.0);
            if cond {
                None
            } else {
                // Take the false branch: jump to the else/endif.
                flow.branch_target(pc)
            }
        }
        PsOp::Ifc => {
            let [a, b] = read_two(regs, &instr.srcs);
            let cond = compare(comp(a, 0), comp(b, 0), instr.control);
            if cond { None } else { flow.branch_target(pc) }
        }
        PsOp::Else => flow.else_target(pc),
        PsOp::EndIf => None,
        PsOp::Loop => {
            if let (Some(dst), Some(src)) = (&instr.dst, instr.srcs.first())
                && let Some(end) = flow.loop_end(pc)
            {
                // The loop spec (aL, aU, aD, aC) comes from an integer
                // constant register: aL = initial, aU = iteration count,
                // aD = step.
                let spec = read_operand_i32(regs, src);
                let a_l = comp_opt(spec, 0);
                let a_u = comp_opt(spec, 1).max(1);
                let a_d = comp_opt(spec, 2);
                if let Some(counter) = regs.loop_regs.get_mut(usize::from(dst.reg_num)) {
                    counter[0] = a_l;
                }
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
            if let Some(counter) = regs.loop_regs.get_mut(usize::from(*reg)) {
                counter[0] = *value;
            }
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
                let count = comp_opt(read_operand_i32(regs, src), 0).max(1);
                if let Some(counter) = regs.loop_regs.get_mut(usize::from(dst.reg_num)) {
                    counter[0] = count;
                }
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
            let [a, b] = read_two(regs, &instr.srcs);
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
                .map_or(0.0, |op| comp(read_operand(regs, op), 0));
            if pred != 0.0 {
                break_from(&mut state.blocks)
            } else {
                None
            }
        }
        PsOp::Call => {
            let label = instr_label(&instr.srcs, &instr.dst);
            call_label(pc, state, flow, label)
        }
        PsOp::CallNz => {
            // callnz l#, bN — call when the boolean source is nonzero.
            let should_call = instr
                .srcs
                .get(1)
                .is_some_and(|op| comp(read_operand(regs, op), 0) != 0.0);
            if should_call {
                let label = instr_label(&instr.srcs, &instr.dst);
                call_label(pc, state, flow, label)
            } else {
                None
            }
        }
        PsOp::Ret => state.call_stack.pop(),
        PsOp::Setp => {
            if let Some(dst) = &instr.dst {
                let [a, b] = read_two(regs, &instr.srcs);
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
                write_operand(regs, dst, result);
            }
            None
        }
        // texld / texkill have no vertex-shader form; Unsupported is
        // unreachable (Create rejects such shaders via the gate).
        PsOp::Tex | PsOp::TexKill | PsOp::TexLdP | PsOp::TexLdB | PsOp::Unsupported(_) => None,
    }
}

/// Helper for `mNxM` matrix ops (the operand count and vector length are the
/// only differences between the five forms).
fn matrix_mul_op(
    regs: &mut VsRegisters,
    instr: &PsInstruction,
    vector_len: usize,
    cols: usize,
) -> Option<usize> {
    if let Some(dst) = &instr.dst
        && let (Some(a), Some(m)) = (instr.srcs.first(), instr.srcs.get(1))
    {
        let vector = read_operand(regs, a);
        let result = matrix_multiply(regs, vector, m, vector_len, cols);
        write_operand(regs, dst, result);
    }
    None
}

/// The `call`/`callnz` label number: the label operand's register number.
#[must_use]
fn instr_label(srcs: &[Operand], dst: &Option<Operand>) -> u16 {
    srcs.first().or(dst.as_ref()).map_or(0, |op| op.reg_num)
}

/// Execute a `call` (shared by `call` / `callnz`): push the return address
/// and jump to the label, or end the shader when the stack is full or the
/// label is undefined.
fn call_label(pc: usize, state: &mut FlowState, flow: &FlowMap, label: u16) -> Option<usize> {
    if state.call_stack.len() >= super::flow::CALL_DEPTH_LIMIT {
        return None;
    }
    state.call_stack.push(pc.saturating_add(1));
    flow.label(label)
}

/// Pop the innermost `loop`/`rep` block and return its break target (one past
/// the matching `endloop`/`endrep`). `None` with an empty stack means "end
/// the shader" (a break outside any loop is invalid vs_2_0).
fn break_from(blocks: &mut Vec<BlockState>) -> Option<usize> {
    blocks.pop().map(|block| match block {
        BlockState::Loop { end, .. } | BlockState::Rep { end, .. } => end.saturating_add(1),
    })
}

/// Execute a vertex shader for one vertex; returns the register-file outputs.
///
/// Semantics mirror the PS interpreter op for op (`slt dst, a, b` is
/// `(a < b)`, `lrp` is `a*b + (1-a)*c`, `cmp` is `(a >= 0) ? b : c`, …).
/// Domain edges use plain IEEE f32 arithmetic (`rcp(0) = +inf`, `rsq(x<0) =
/// 1/sqrt(|x|)`) — the same documented contract as the pixel stage. Flow
/// control runs through the shared [`FlowMap`]/[`FlowState`] machinery with a
/// bounded step budget.
#[must_use]
pub fn run_vertex_shader(program: &VsProgram<'_>, input: &VsVertexInput) -> VsOutput {
    let mut regs = VsRegisters {
        temp: [[0.0; 4]; VS_TEMP_COUNT],
        constants: program.constants,
        input: input.v,
        addr: [0.0; 4],
        const_bool: program.bool_constants,
        loop_regs: program.int_constants,
        pred: [0.0; 4],
        rast_out: [[0.0; 4]; VS_RASTOUT_COUNT],
        attr_out: [[0.0; 4]; VS_ATTROUT_COUNT],
        texcrd_out: [[0.0; 4]; VS_TEXCRDOUT_COUNT],
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
        let jump = execute_one(pc, instr, &mut regs, &flow, &mut state);
        match jump {
            Some(target) => pc = target,
            None => pc = pc.saturating_add(1),
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
