//! D3D9 shader bytecode tokenizer + parsed shader model.
//!
//! Parses the ps_2_0 / vs_2_0 DWORD token stream at `Create*Shader` time into a
//! typed instruction list the pixel-shader interpreter (`d3d9_render.rs`)
//! executes. Token masks and opcode values are the D3D9 ABI: every constant
//! below is transcribed from the Windows SDK `d3d9shader.h` /
//! `d3d9types.h` definitions (verified against the mingw-w64 shipped copies of
//! `d3d9.h` / `d3d9types.h` on this machine), which is also the format Wine's
//! d3d9 shader reader consumes. Do not "fix" these from memory — slot values
//! drift across D3D9 header versions; the authoritative values are the ones here.
//!
//! Token layout (D3D9, from d3d9shader.h):
//! - Version token: `0xFFFF0000 | (major << 8) | minor` for pixel shaders
//!   (`D3DPS_VERSION`), `0xFFFE0000 | …` for vertex shaders (`D3DVS_VERSION`).
//! - Instruction token: bits 0-15 opcode (`D3DSIO_*`), bits 16-23 opcode-
//!   specific control, bit 28 predicated (`D3DSHADER_INSTRUCTION_PREDICATED`).
//!   There is no length field — the number of following tokens is implied by
//!   the opcode (the tokenizer walks with the operand-count table below, the
//!   same way Wine's `shader_get_next_instruction` does).
//! - Operand token: bits 0-10 register number (`D3DSP_REGNUM_MASK`), register
//!   type split across bits 28-30 (`D3DSP_REGTYPE`, 3 bits) and bits 11-12
//!   (`D3DSP_REGTYPE_MASK2` = 0x1800), source modifier in bits 24-27
//!   (`D3DSP_SRCMOD`), destination modifier in bits 20-22 (`D3DSP_DSTMOD`),
//!   write mask in bits 16-19 (`D3DSP_WRITEMASK_*`), swizzle in bits 16-23
//!   (2 bits per output component, `D3DSP_NOSWIZZLE` = 0x00E40000).
//! - Comment token: opcode `D3DSIO_COMMENT` (0xFFFE), bits 16-29 hold the
//!   number of DWORDs of comment payload that follow.
// D3DSIO_M4x4 / TEXM3x2TEX etc. are the header's exact spelling (mixed case);
// renaming would obscure the d3d9types.h source, so the lint is expected.
#![expect(non_upper_case_globals)]

use anyhow::{Context, Result};

/// Version-tag for pixel shaders (`D3DPS_VERSION` tag bits).
const D3DPS_VERSION_TAG: u32 = 0xFFFF_0000;
/// Version-tag for vertex shaders (`D3DVS_VERSION` tag bits).
const D3DVS_VERSION_TAG: u32 = 0xFFFE_0000;

/// Opcode field of an instruction token (`token & 0xFFFF`).
pub const OPCODE_FIELD_MASK: u32 = 0x0000_FFFF;
/// Opcode-specific control (`D3DSP_OPCODESPECIFICCONTROL_MASK`).
const D3DSP_OPCODESPECIFICCONTROL_SHIFT: u32 = 16;
const D3DSP_OPCODESPECIFICCONTROL_MASK: u32 = 0x00FF_0000;
/// Comment payload length (`D3DSI_COMMENTSIZE_MASK` = 0x7FFF << 16).
pub const COMMENTSIZE_FIELD_SHIFT: u32 = 16;
pub const COMMENTSIZE_FIELD_MASK: u32 = 0x7FFF_0000;
/// Predicated-instruction flag (bit 28).
const D3DSHADER_INSTRUCTION_PREDICATED: u32 = 0x1000_0000;
/// Public mirror of the predicated-flag bit for the guest bytecode walker
/// (`d3d9/shader.rs`), which must count the extra predicate operand token.
pub const PREDICATED_INSTRUCTION_MASK: u32 = 0x1000_0000;

/// Register number (`D3DSP_REGNUM_MASK`).
const D3DSP_REGNUM_MASK: u32 = 0x0000_07FF;
/// Register type, low 3 bits (`D3DSP_REGTYPE_MASK` in bits 28-30).
const D3DSP_REGTYPE_SHIFT: u32 = 28;
const D3DSP_REGTYPE_MASK: u32 = 0x7 << D3DSP_REGTYPE_SHIFT;
/// Register type, high 2 bits (`D3DSP_REGTYPE_MASK2` = 0x1800 in bits 11-12).
const D3DSP_REGTYPE_SHIFT2: u32 = 8;
const D3DSP_REGTYPE_MASK2: u32 = 0x0000_1800;
/// Relative-addressing flag (`D3DSHADER_ADDRESSMODE_MASK` = bit 13): when set,
/// the operand's register number is offset by the `a0.x` address register
/// (`c[a0.x + n]` vs `c[n]`). L5 decodes and executes this form.
const RELATIVE_ADDRESSING_SHIFT: u32 = 13;
const RELATIVE_ADDRESSING_MASK: u32 = 1 << RELATIVE_ADDRESSING_SHIFT;
/// Source modifier (`D3DSP_SRCMOD_MASK` = 0xF << 24).
const D3DSP_SRCMOD_SHIFT: u32 = 24;
const D3DSP_SRCMOD_MASK: u32 = 0x0F << D3DSP_SRCMOD_SHIFT;
/// Destination modifier (`D3DSP_DSTMOD_MASK` = 0xF << 20).
const D3DSP_DSTMOD_SHIFT: u32 = 20;
const D3DSP_DSTMOD_MASK: u32 = 0x0F << D3DSP_DSTMOD_SHIFT;
/// Write mask (`D3DSP_WRITEMASK_ALL` = 0xF << 16).
const D3DSP_DSTWRITEMASK_SHIFT: u32 = 16;
const D3DSP_DSTWRITEMASK_MASK: u32 = 0x0F << D3DSP_DSTWRITEMASK_SHIFT;
/// Swizzle field (2 bits per output component, `D3DSP_SWIZZLE_MASK`).
const D3DSP_SWIZZLE_SHIFT: u32 = 16;

/// `D3DSHADER_PARAM_SRCMOD_TYPE` values (`D3DSPSM_*`).
pub const D3DSPSM_NONE: u8 = 0;
pub const D3DSPSM_NEG: u8 = 1;
pub const D3DSPSM_BIAS: u8 = 2;
pub const D3DSPSM_BIASNEG: u8 = 3;
pub const D3DSPSM_SIGN: u8 = 4;
pub const D3DSPSM_SIGNNEG: u8 = 5;
pub const D3DSPSM_COMP: u8 = 6;
pub const D3DSPSM_X2: u8 = 7;
pub const D3DSPSM_X2NEG: u8 = 8;
pub const D3DSPSM_DZ: u8 = 9;
pub const D3DSPSM_DW: u8 = 10;
pub const D3DSPSM_ABS: u8 = 11;
pub const D3DSPSM_ABSNEG: u8 = 12;
pub const D3DSPSM_NOT: u8 = 13;

/// `D3DSHADER_PARAM_DSTMOD_TYPE` values (`D3DSPDM_*`).
pub const D3DSPDM_NONE: u8 = 0;
pub const D3DSPDM_SATURATE: u8 = 1;
pub const D3DSPDM_PARTIALPRECISION: u8 = 2;
pub const D3DSPDM_MSAMPCENTROID: u8 = 4;

/// `D3DSHADER_COMPARISON` values (`D3DSPC_*`), carried in the opcode-specific
/// control field of `ifc` / `breakc` / `setp` (from d3d9types.h).
pub const D3DSPC_GT: u8 = 1;
pub const D3DSPC_EQ: u8 = 2;
pub const D3DSPC_GE: u8 = 3;
pub const D3DSPC_LT: u8 = 4;
pub const D3DSPC_NE: u8 = 5;
pub const D3DSPC_LE: u8 = 6;

/// `D3DSHADER_PARAM_REGISTER_TYPE` values (`D3DSPR_*`).
///
/// `ADDR` and `TEXTURE` share value 3 (the register file is interpreted by
/// context: the address register in vertex shaders, texture coordinates in
/// pixel shaders); `OUTPUT` and `TEXCRDOUT` share value 6. `D3DSPR_SAMPLER`
/// is 10 (not 11 as often misremembered).
pub const D3DSPR_TEMP: u8 = 0;
pub const D3DSPR_INPUT: u8 = 1;
pub const D3DSPR_CONST: u8 = 2;
pub const D3DSPR_TEXTURE: u8 = 3;
pub const D3DSPR_RASTOUT: u8 = 4;
pub const D3DSPR_ATTROUT: u8 = 5;
pub const D3DSPR_TEXCRDOUT: u8 = 6;
pub const D3DSPR_CONSTINT: u8 = 7;
pub const D3DSPR_COLOROUT: u8 = 8;
pub const D3DSPR_DEPTHOUT: u8 = 9;
pub const D3DSPR_SAMPLER: u8 = 10;
pub const D3DSPR_CONST2: u8 = 11;
pub const D3DSPR_CONST3: u8 = 12;
pub const D3DSPR_CONST4: u8 = 13;
pub const D3DSPR_CONSTBOOL: u8 = 14;
pub const D3DSPR_LOOP: u8 = 15;
pub const D3DSPR_TEMPFLOAT16: u8 = 16;
pub const D3DSPR_MISCTYPE: u8 = 17;
pub const D3DSPR_LABEL: u8 = 18;
pub const D3DSPR_PREDICATE: u8 = 19;

/// `D3DSHADER_INSTRUCTION_OPCODE_TYPE` values (`D3DSIO_*`).
///
/// Values transcribed from d3d9types.h. Note the ps_2_0 `texld` instruction is
/// opcode `D3DSIO_TEX` (66), `D3DSIO_DEF` is 81 and `D3DSIO_CMP` is 88 —
/// all three are frequently misquoted from memory.
pub const D3DSIO_NOP: u32 = 0;
pub const D3DSIO_MOV: u32 = 1;
pub const D3DSIO_ADD: u32 = 2;
pub const D3DSIO_SUB: u32 = 3;
pub const D3DSIO_MAD: u32 = 4;
pub const D3DSIO_MUL: u32 = 5;
pub const D3DSIO_RCP: u32 = 6;
pub const D3DSIO_RSQ: u32 = 7;
pub const D3DSIO_DP3: u32 = 8;
pub const D3DSIO_DP4: u32 = 9;
pub const D3DSIO_MIN: u32 = 10;
pub const D3DSIO_MAX: u32 = 11;
pub const D3DSIO_SLT: u32 = 12;
pub const D3DSIO_SGE: u32 = 13;
pub const D3DSIO_EXP: u32 = 14;
pub const D3DSIO_LOG: u32 = 15;
pub const D3DSIO_LIT: u32 = 16;
pub const D3DSIO_DST: u32 = 17;
pub const D3DSIO_LRP: u32 = 18;
pub const D3DSIO_FRC: u32 = 19;
pub const D3DSIO_M4x4: u32 = 20;
pub const D3DSIO_M4x3: u32 = 21;
pub const D3DSIO_M3x4: u32 = 22;
pub const D3DSIO_M3x3: u32 = 23;
pub const D3DSIO_M3x2: u32 = 24;
pub const D3DSIO_CALL: u32 = 25;
pub const D3DSIO_CALLNZ: u32 = 26;
pub const D3DSIO_LOOP: u32 = 27;
pub const D3DSIO_RET: u32 = 28;
pub const D3DSIO_ENDLOOP: u32 = 29;
pub const D3DSIO_LABEL: u32 = 30;
pub const D3DSIO_DCL: u32 = 31;
pub const D3DSIO_POW: u32 = 32;
pub const D3DSIO_CRS: u32 = 33;
pub const D3DSIO_SGN: u32 = 34;
pub const D3DSIO_ABS: u32 = 35;
pub const D3DSIO_NRM: u32 = 36;
pub const D3DSIO_SINCOS: u32 = 37;
pub const D3DSIO_REP: u32 = 38;
pub const D3DSIO_ENDREP: u32 = 39;
pub const D3DSIO_IF: u32 = 40;
pub const D3DSIO_IFC: u32 = 41;
pub const D3DSIO_ELSE: u32 = 42;
pub const D3DSIO_ENDIF: u32 = 43;
pub const D3DSIO_BREAK: u32 = 44;
pub const D3DSIO_BREAKC: u32 = 45;
pub const D3DSIO_MOVA: u32 = 46;
pub const D3DSIO_DEFB: u32 = 47;
pub const D3DSIO_DEFI: u32 = 48;
pub const D3DSIO_TEXCOORD: u32 = 64;
pub const D3DSIO_TEXKILL: u32 = 65;
/// The ps_2_0 `texld` opcode (`D3DSIO_TEX`).
pub const D3DSIO_TEX: u32 = 66;
pub const D3DSIO_TEXBEM: u32 = 67;
pub const D3DSIO_TEXBEML: u32 = 68;
pub const D3DSIO_TEXREG2AR: u32 = 69;
pub const D3DSIO_TEXREG2GB: u32 = 70;
pub const D3DSIO_TEXM3x2PAD: u32 = 71;
pub const D3DSIO_TEXM3x2TEX: u32 = 72;
pub const D3DSIO_TEXM3x3PAD: u32 = 73;
pub const D3DSIO_TEXM3x3TEX: u32 = 74;
pub const D3DSIO_TEXM3x3DIFF: u32 = 75;
pub const D3DSIO_TEXM3x3SPEC: u32 = 76;
pub const D3DSIO_TEXM3x3VSPEC: u32 = 77;
pub const D3DSIO_EXPP: u32 = 78;
pub const D3DSIO_LOGP: u32 = 79;
pub const D3DSIO_CND: u32 = 80;
pub const D3DSIO_DEF: u32 = 81;
pub const D3DSIO_TEXREG2RGB: u32 = 82;
pub const D3DSIO_TEXDP3TEX: u32 = 83;
pub const D3DSIO_TEXM3x2DEPTH: u32 = 84;
pub const D3DSIO_TEXDP3: u32 = 85;
pub const D3DSIO_TEXM3x3: u32 = 86;
pub const D3DSIO_TEXDEPTH: u32 = 87;
pub const D3DSIO_CMP: u32 = 88;
pub const D3DSIO_BEM: u32 = 89;
pub const D3DSIO_DP2ADD: u32 = 90;
pub const D3DSIO_DSX: u32 = 91;
pub const D3DSIO_DSY: u32 = 92;
pub const D3DSIO_TEXLDD: u32 = 93;
pub const D3DSIO_SETP: u32 = 94;
pub const D3DSIO_TEXLDL: u32 = 95;
pub const D3DSIO_BREAKP: u32 = 96;
pub const D3DSIO_PHASE: u32 = 0xFFFD;
pub const D3DSIO_COMMENT: u32 = 0xFFFE;
pub const D3DSIO_END: u32 = 0xFFFF;

/// `D3DSI_TEXLD_PROJECT` — the ps_2_a/b `texld` opcode-specific control for
/// the projective form (`texldp`: sample at `u/w`, `v/w`).
const D3DSI_TEXLD_PROJECT: u32 = 0x1;
/// `D3DSI_TEXLD_BIAS` — the ps_2_a/b `texld` control for the bias form
/// (`texldb`: the `w` component biases the mip selection).
const D3DSI_TEXLD_BIAS: u32 = 0x2;

/// `D3DSP_WRITEMASK_0` — write-mask bit for component 0 (x).
pub const D3DSP_WRITEMASK_0: u8 = 0x1;
/// `D3DSP_WRITEMASK_1` — write-mask bit for component 1 (y).
pub const D3DSP_WRITEMASK_1: u8 = 0x2;
/// `D3DSP_WRITEMASK_2` — write-mask bit for component 2 (z).
pub const D3DSP_WRITEMASK_2: u8 = 0x4;
/// `D3DSP_WRITEMASK_3` — write-mask bit for component 3 (w).
pub const D3DSP_WRITEMASK_3: u8 = 0x8;

/// Bound on the number of DWORD tokens one shader may contain.
///
/// ps_2_0 caps instruction counts in the dozens; 4096 is a generous ceiling
/// that still bounds guest memory reads from a hostile/looping bytecode.
pub const MAX_SHADER_TOKENS: usize = 4096;

/// ps_2_0 register-file sizes (the interpreter's contract).
pub const PS_TEMP_COUNT: usize = 12;
pub const PS_CONST_COUNT: usize = 32;
pub const PS_INPUT_COUNT: usize = 2;
pub const PS_SAMPLER_COUNT: usize = 4;
/// vs_2_0 constant-register file size (`MaxVertexShaderConst`).
pub const VS_CONST_COUNT: usize = 256;
/// vs_2_0 boolean constant-register file size (`b0..b15`).
pub const VS_BOOL_CONST_COUNT: usize = 16;
/// vs_2_0 integer/loop-register file size (`i0..i3`).
pub const VS_INT_CONST_COUNT: usize = 4;

/// Which programmable stage a shader targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderKind {
    /// Pixel shader (`ps_2_x`; executed by the fragment stage).
    Pixel,
    /// Vertex shader (`vs_2_x`; stored/bound, execution not yet implemented).
    Vertex,
}

/// A created shader object's host-side record.
#[derive(Debug, Clone)]
pub struct ShaderRecord {
    /// The shader object's guest VA (also the `IDirect3D*Shader9` pointer).
    pub handle: u64,
    /// Pixel vs vertex.
    pub kind: ShaderKind,
    /// Owned copy of the guest bytecode (the guest buffer is transient).
    pub bytecode: Vec<u32>,
    /// Tokenized form (parsed at `Create*Shader` time).
    pub parsed: ParsedShader,
}

/// Register-file selector of one operand.
///
/// The enum is shared between the pixel and vertex stages — the shader kind
/// decides the meaning of the shared type values (`Texture` = `tN` texture
/// coordinates in pixel shaders, the `a0` address register in vertex shaders).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegType {
    /// `rN` — temporary register file.
    Temp,
    /// `vN` — interpolated input register file (pixel shaders) / the
    /// FVF-stream vertex input registers `v0..v15` (vertex shaders).
    Input,
    /// `cN` — constant register file.
    Const,
    /// `tN` — texture-coordinate register file (pixel shaders) / the `a0`
    /// address register (vertex shaders; written by `mova`, which is L5).
    Texture,
    /// `oC0` — color output register (pixel shader destination).
    ColorOut,
    /// `oDepth` — depth output register (pixel shader; stored, not applied).
    DepthOut,
    /// `oPos`/`oFog`/`oPts` — rasterizer-output registers (vertex shaders).
    RastOut,
    /// `oD0`/`oD1` — vertex color output registers.
    AttrOut,
    /// `oT0..oT7` — vertex texture-coordinate output registers.
    TexcrdOut,
    /// `bN` — boolean constant registers (vertex shaders).
    ConstBool,
    /// `iN` — integer loop registers (vertex shaders; the loop ops are L5).
    Loop,
    /// `sN` — sampler register file (texld sources).
    Sampler,
    /// `l#` — label register (call/callnz targets; vertex shaders).
    Label,
    /// `p0` — the predicate register (setp destinations, predication sources).
    Predicate,
    /// Unmodeled register file (raw value preserved).
    Other(u8),
}

/// One decoded shader operand (register + modifiers).
#[derive(Debug, Clone, Copy)]
pub struct Operand {
    /// Register file.
    pub reg_type: RegType,
    /// Register number within the file.
    pub reg_num: u16,
    /// Whether the register number is relative (`c[a0.x + n]`): the effective
    /// index adds the address register `a0.x` (vs_2_0 relative addressing).
    pub relative: bool,
    /// Per-output-component source select, each `0..3` (`D3DSP_SWIZZLE`).
    pub swizzle: [u8; 4],
    /// Source modifier (`D3DSPSM_*`); 0 = none.
    pub src_mod: u8,
    /// Destination modifier (`D3DSPDM_*`); 0 = none.
    pub dst_mod: u8,
    /// Write mask: bit 0 = x, bit 1 = y, bit 2 = z, bit 3 = w.
    pub write_mask: u8,
}

impl Operand {
    /// Whether this operand targets the given register file (for validation).
    #[must_use]
    pub fn is_reg(&self, reg: RegType) -> bool {
        self.reg_type == reg
    }
}

/// Decoded instruction opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsOp {
    /// `nop`.
    Nop,
    /// `mov dst, s0`.
    Mov,
    /// `add dst, s0, s1`.
    Add,
    /// `sub dst, s0, s1`.
    Sub,
    /// `mad dst, s0, s1, s2`.
    Mad,
    /// `mul dst, s0, s1`.
    Mul,
    /// `rcp dst, s0` (1/s0.x replicated).
    Rcp,
    /// `rsq dst, s0` (1/sqrt(|s0.x|) replicated).
    Rsq,
    /// `dp3 dst, s0, s1` (dot of .xyz replicated).
    Dp3,
    /// `dp4 dst, s0, s1` (dot of .xyzw replicated).
    Dp4,
    /// `min dst, s0, s1`.
    Min,
    /// `max dst, s0, s1`.
    Max,
    /// `slt dst, s0, s1` (1.0 where s0 < s1, else 0.0).
    Slt,
    /// `sge dst, s0, s1` (1.0 where s0 >= s1, else 0.0).
    Sge,
    /// `exp dst, s0` (dst.x = 2^s0.x; dst.yzw = s0.yzw).
    Exp,
    /// `log dst, s0` (dst.x = log2(s0.x); dst.yzw = s0.yzw).
    Log,
    /// `lrp dst, s0, s1, s2` (s0*s1 + (1-s0)*s2).
    Lrp,
    /// `frc dst, s0` (s0 - floor(s0)).
    Frc,
    /// `cmp dst, s0, s1, s2` (s0 >= 0 ? s1 : s2).
    Cmp,
    /// `texld dst, s0, s1` (D3DSIO_TEX — sample sampler s1 at uv s0.xy).
    Tex,
    /// `texkill s0` (discard the fragment if any component of s0 < 0).
    TexKill,
    /// `def cN, f0..f3` (compile-time constant; no execution).
    Def,
    /// `dcl ...` (register declaration; no execution).
    Dcl,
    /// `end`.
    End,
    // ── L5 vs_2_0 arithmetic (the vs_2_0-only advanced ops) ─────────────
    /// `m4x4 dst, v, m` — row-vector × 4x4 matrix (dst.i = v·row_i).
    M4x4,
    /// `m4x3 dst, v, m` — row-vector × 4x3 (dst.xyz written).
    M4x3,
    /// `m3x4 dst, v, m` — 3-vector × 3x4 (dst.xyzw written).
    M3x4,
    /// `m3x3 dst, v, m` — 3-vector × 3x3 (dst.xyz written).
    M3x3,
    /// `m3x2 dst, v, m` — 3-vector × 3x2 (dst.xy written).
    M3x2,
    /// `dst dst, s0, s1` — distance-vector helper (dst = (1, s0.y·s1.y, s0.z, s1.w)).
    Dst,
    /// `lit dst, s0` — lighting (diffuse + specular powers from N·L, N·H).
    Lit,
    /// `pow dst, s0, s1` — s0.x^s1.x replicated to all channels.
    Pow,
    /// `crs dst, s0, s1` — cross product of the .xyz components.
    Crs,
    /// `sgn dst, s0, s1, s2` — component sign of s0 (s1 = -1, s2 = +1 conventions).
    Sgn,
    /// `abs dst, s0` — per-component absolute value.
    Abs,
    /// `nrm dst, s0` — normalize s0.xyz.
    Nrm,
    /// `sincos dst, s0, s1, s2` — dst.xy = (cos, sin) of s0.x (s1/s2 unused).
    SinCos,
    /// `dp2add dst, s0, s1, s2` — 2-dot + scalar: s0.x·s1.x + s0.y·s1.y + s2.x.
    Dp2Add,
    /// `mova a0, s0` — write the address register (drives relative addressing).
    Mova,
    // ── L5 flow control (vs_2_0; ps_2_x subsets use the comparison forms) ─
    /// `if bN` — skip to the matching else/endif when bN.x == 0.
    If,
    /// `ifc s0, s1` — conditional with the opcode-control comparison.
    Ifc,
    /// `else` — the false branch of an if/ifc block.
    Else,
    /// `endif` — ends an if/ifc block.
    EndIf,
    /// `loop iN, cI` — repeat the body aU times, iN stepping aL→aL+(aU-1)·aD.
    Loop,
    /// `endloop` — step the loop counter and jump back while iterations remain.
    EndLoop,
    /// `rep iN, cI` — repeat the body cI.x times.
    Rep,
    /// `endrep` — jump back while the repeat count remains.
    EndRep,
    /// `break` — exit the innermost loop/rep.
    Break,
    /// `breakc s0, s1` — break when the opcode-control comparison holds.
    BreakC,
    /// `breakp p0` — break when p0.x != 0.
    BreakP,
    /// `call l#` — call the label (push the return address).
    Call,
    /// `callnz l#, bN` — call when bN.x != 0.
    CallNz,
    /// `label l#` — a call target (no execution).
    Label,
    /// `ret` — return from the innermost call (top-level ret ends the shader).
    Ret,
    /// `setp p0, s0, s1` — per-component comparison → the predicate register.
    Setp,
    /// `defb bN, bool` (compile-time boolean constant; no execution).
    DefB,
    /// `defi iN, int` (compile-time integer constant; no execution).
    DefI,
    /// `texld dst, tN, sM` with the project control (texldp — sample at u/w, v/w).
    TexLdP,
    /// `texld dst, tN, sM` with the bias control (texldb — mip bias; no-op on
    /// the single-level point sampler, documented).
    TexLdB,
    /// Structurally valid but not executable (not yet implemented).
    Unsupported(u32),
}

/// One decoded instruction.
#[derive(Debug, Clone)]
pub struct PsInstruction {
    /// The decoded opcode.
    pub op: PsOp,
    /// Destination operand (absent for `texkill` and no-operand ops).
    pub dst: Option<Operand>,
    /// Source operands in operand-token order.
    pub srcs: Vec<Operand>,
    /// Sampler texture type (`D3DSTT_*`) for `dcl` on a sampler register.
    pub tex_type: Option<u32>,
    /// Opcode-specific control bits (the `ifc`/`breakc`/`setp` comparison,
    /// the `dcl` texture type). 0 when the opcode carries none.
    pub control: u8,
    /// Whether the instruction is predicated (bit 28): it executes only when
    /// the predicate register `p0.x != 0`; the first operand token is `p0`.
    pub predicated: bool,
    /// True for the terminating `end` instruction.
    pub end: bool,
}

/// A fully tokenized shader.
#[derive(Debug, Clone)]
pub struct ParsedShader {
    /// Pixel or vertex.
    pub kind: ShaderKind,
    /// Raw version token (`D3DPS_VERSION`/`D3DVS_VERSION` value).
    pub version: u32,
    /// Instructions in token order (includes the `end` terminator).
    pub instructions: Vec<PsInstruction>,
    /// `def cN, …` constants in declaration order.
    pub constants: Vec<(u32, [f32; 4])>,
    /// `defb bN, bool` constants in declaration order.
    pub bool_constants: Vec<(u32, bool)>,
    /// `defi iN, int` constants in declaration order.
    pub int_constants: Vec<(u32, i32)>,
}

impl ParsedShader {
    /// Whether every instruction is in the executable subset.
    ///
    /// The interpreters implement the arithmetic core plus the L5 wave: the
    /// vs_2_0 matrix/lighting ops (`M4x4`..`M3x2`, `DST`, `LIT`, `POW`, `CRS`,
    /// `SGN`, `ABS`, `NRM`, `SINCOS`, `DP2ADD`, `MOVA`), ALL flow control
    /// (`IF`/`IFC`/`ELSE`/`ENDIF`, `LOOP`/`ENDLOOP`, `REP`/`ENDREP`,
    /// `BREAK`/`BREAKC`/`BREAKP`, `CALL`/`CALLNZ`/`LABEL`/`RET`, `SETP`,
    /// predication), the `DEFB`/`DEFI` constant definitions, and the ps_2_a/b
    /// texld forms (`TEXLDP`/`TEXLDB`). Everything else parses but is
    /// rejected at `Create*Shader` with `D3DERR_INVALIDCALL`.
    #[must_use]
    pub fn is_fully_executable(&self) -> bool {
        self.instructions
            .iter()
            .all(|instr| !matches!(instr.op, PsOp::Unsupported(_)))
    }
}

/// Decode the version token; `None` for a non-shader-2 token.
///
/// Accepts the ps_2_x family for pixel shaders (`D3DPS_VERSION(2, n)` with
/// `n` in 0..=2 or 0xFE — ps_2_0 / ps_2_a / ps_2_b / ps_2_sw) and the vs_2_x
/// family for vertex shaders (`D3DVS_VERSION(2, 0/1)`).
pub fn decode_shader_version(token: u32) -> Option<(ShaderKind, u32, u32)> {
    let tag = token & 0xFFFF_0000;
    let major = (token >> 8) & 0xFF;
    let minor = token & 0xFF;
    match tag {
        D3DPS_VERSION_TAG if major == 2 && matches!(minor, 0..=2 | 0xFE) => {
            Some((ShaderKind::Pixel, major, minor))
        }
        D3DVS_VERSION_TAG if major == 2 && matches!(minor, 0..=1) => {
            Some((ShaderKind::Vertex, major, minor))
        }
        _ => None,
    }
}

/// Decode an operand token into the typed [`Operand`].
fn parse_operand(token: u32) -> Operand {
    // The combined value is 5 bits (3 type bits + 2 type bits), so the
    // narrowing conversion below cannot fail.
    let reg_type_raw = u8::try_from(
        ((token & D3DSP_REGTYPE_MASK) >> D3DSP_REGTYPE_SHIFT)
            | ((token & D3DSP_REGTYPE_MASK2) >> D3DSP_REGTYPE_SHIFT2),
    )
    .unwrap_or(0);
    let reg_type = match reg_type_raw {
        D3DSPR_TEMP => RegType::Temp,
        D3DSPR_INPUT => RegType::Input,
        D3DSPR_CONST => RegType::Const,
        D3DSPR_TEXTURE => RegType::Texture,
        D3DSPR_RASTOUT => RegType::RastOut,
        D3DSPR_ATTROUT => RegType::AttrOut,
        D3DSPR_TEXCRDOUT => RegType::TexcrdOut,
        D3DSPR_COLOROUT => RegType::ColorOut,
        D3DSPR_DEPTHOUT => RegType::DepthOut,
        D3DSPR_CONSTBOOL => RegType::ConstBool,
        D3DSPR_LOOP => RegType::Loop,
        D3DSPR_SAMPLER => RegType::Sampler,
        D3DSPR_LABEL => RegType::Label,
        D3DSPR_PREDICATE => RegType::Predicate,
        other => RegType::Other(other),
    };
    // Register number: D3DSP_REGNUM_MASK (bits 0-10). For register files with
    // a nonzero type the top regtype bits sit in 8-10, so common files (temp,
    // const ≤ 31, sampler ≤ 15) only ever use bits 0-7.
    let reg_num = u16::try_from(token & D3DSP_REGNUM_MASK).unwrap_or(0);
    let relative = token & RELATIVE_ADDRESSING_MASK != 0;
    let src_mod = u8::try_from((token & D3DSP_SRCMOD_MASK) >> D3DSP_SRCMOD_SHIFT).unwrap_or(0);
    let dst_mod = u8::try_from((token & D3DSP_DSTMOD_MASK) >> D3DSP_DSTMOD_SHIFT).unwrap_or(0);
    let write_mask =
        u8::try_from((token & D3DSP_DSTWRITEMASK_MASK) >> D3DSP_DSTWRITEMASK_SHIFT).unwrap_or(0);
    let swizzle = [
        u8::try_from((token >> D3DSP_SWIZZLE_SHIFT) & 0x3).unwrap_or(0),
        u8::try_from((token >> (D3DSP_SWIZZLE_SHIFT + 2)) & 0x3).unwrap_or(0),
        u8::try_from((token >> (D3DSP_SWIZZLE_SHIFT + 4)) & 0x3).unwrap_or(0),
        u8::try_from((token >> (D3DSP_SWIZZLE_SHIFT + 6)) & 0x3).unwrap_or(0),
    ];
    Operand {
        reg_type,
        reg_num,
        relative,
        swizzle,
        src_mod,
        dst_mod,
        write_mask,
    }
}

/// Number of DWORD tokens that follow an instruction's opcode token.
///
/// The D3D9 instruction token carries no length field — the count is implied
/// by the opcode, as in Wine's shader_get_next_instruction. `None` for an
/// unknown opcode. The counts are derived from the ps_2_0 / vs_2_0 operand
/// counts (`texld` has 3 operands in ps_2_0: dst + texture-coordinate +
/// sampler; `def` carries an operand + 4 float DWORDs).
#[must_use]
pub fn instruction_payload_len(opcode: u32) -> Option<usize> {
    match opcode {
        D3DSIO_NOP | D3DSIO_RET | D3DSIO_END | D3DSIO_PHASE | D3DSIO_ELSE | D3DSIO_ENDIF
        | D3DSIO_BREAK | D3DSIO_ENDLOOP | D3DSIO_ENDREP => Some(0),
        D3DSIO_DEF => Some(5), // operand + 4 constant DWORDs
        D3DSIO_DCL | D3DSIO_TEXKILL | D3DSIO_TEXCOORD | D3DSIO_IF | D3DSIO_LABEL
        | D3DSIO_BREAKP | D3DSIO_EXPP | D3DSIO_LOGP | D3DSIO_TEXBEM | D3DSIO_TEXBEML
        | D3DSIO_TEXREG2AR | D3DSIO_TEXREG2GB | D3DSIO_TEXREG2RGB | D3DSIO_TEXDP3
        | D3DSIO_TEXM3x3 | D3DSIO_TEXM3x3DIFF | D3DSIO_TEXM3x3SPEC | D3DSIO_TEXM3x3VSPEC
        | D3DSIO_TEXM3x2PAD | D3DSIO_TEXM3x3PAD | D3DSIO_TEXDEPTH => Some(1),
        D3DSIO_MOV | D3DSIO_RCP | D3DSIO_RSQ | D3DSIO_EXP | D3DSIO_LOG | D3DSIO_LIT
        | D3DSIO_FRC | D3DSIO_ABS | D3DSIO_NRM | D3DSIO_MOVA | D3DSIO_TEXLDL | D3DSIO_DSX
        | D3DSIO_DSY | D3DSIO_LOOP | D3DSIO_REP | D3DSIO_CALL | D3DSIO_DEFB | D3DSIO_DEFI
        | D3DSIO_IFC | D3DSIO_BREAKC => Some(2),
        D3DSIO_ADD | D3DSIO_SUB | D3DSIO_MUL | D3DSIO_DP3 | D3DSIO_DP4 | D3DSIO_MIN
        | D3DSIO_MAX | D3DSIO_SLT | D3DSIO_SGE | D3DSIO_M4x4 | D3DSIO_M4x3 | D3DSIO_M3x4
        | D3DSIO_M3x3 | D3DSIO_M3x2 | D3DSIO_DST | D3DSIO_POW | D3DSIO_CRS | D3DSIO_SGN
        | D3DSIO_TEXM3x2TEX | D3DSIO_TEXM3x3TEX | D3DSIO_TEXDP3TEX | D3DSIO_TEXM3x2DEPTH
        | D3DSIO_BEM | D3DSIO_TEX | D3DSIO_CALLNZ | D3DSIO_SETP => Some(3),
        D3DSIO_MAD | D3DSIO_LRP | D3DSIO_CMP | D3DSIO_CND | D3DSIO_SINCOS | D3DSIO_DP2ADD
        | D3DSIO_TEXLDD => Some(4),
        _ => None,
    }
}

/// Decode one instruction starting at `tokens[0]` (the opcode token).
///
/// Returns the decoded instruction and the number of DWORD tokens consumed
/// (opcode token + payload). `Err` for an unknown opcode or a payload that
/// runs past `tokens`.
fn parse_instruction(_kind: ShaderKind, tokens: &[u32]) -> Result<(PsInstruction, usize)> {
    let token = *tokens
        .first()
        .context("empty instruction (missing opcode token)")?;
    let opcode = token & OPCODE_FIELD_MASK;
    // Predication (ps_3_0/vs_3_0 feature, but the ps_2_a/vs_2_0 token form is
    // the same): the first operand token is the predicate register p0 and the
    // instruction runs only while p0.x != 0.
    let predicated = token & D3DSHADER_INSTRUCTION_PREDICATED != 0;
    let control = (token & D3DSP_OPCODESPECIFICCONTROL_MASK) >> D3DSP_OPCODESPECIFICCONTROL_SHIFT;

    let Some(payload_len) = instruction_payload_len(opcode) else {
        anyhow::bail!("unknown shader opcode {opcode:#06x}");
    };
    // A predicated instruction carries the predicate operand ahead of its
    // normal dst/src operands.
    let pred_operands = usize::from(predicated);
    let total = payload_len
        .checked_add(1)
        .and_then(|t| t.checked_add(pred_operands))
        .context("instruction length overflow")?;
    let payload = tokens.get(1..total).context("operand runs past bytecode")?;

    if predicated {
        let predicate = payload
            .first()
            .copied()
            .map(parse_operand)
            .context("missing predicate operand token")?;
        if predicate.reg_type != RegType::Predicate {
            anyhow::bail!("predicated instruction must target the p0 register");
        }
    }

    // Map to the executable op + validate structure.
    let decode_operand = |idx: usize| -> Result<Operand> {
        payload
            .get(idx.saturating_add(pred_operands))
            .copied()
            .map(parse_operand)
            .context("missing operand token")
    };
    let (op, dst, src_count, tex_type) = match opcode {
        D3DSIO_NOP => (PsOp::Nop, None, 0, None),
        D3DSIO_MOV => (PsOp::Mov, Some(decode_operand(0)?), 1, None),
        D3DSIO_ADD => (PsOp::Add, Some(decode_operand(0)?), 2, None),
        D3DSIO_SUB => (PsOp::Sub, Some(decode_operand(0)?), 2, None),
        D3DSIO_MAD => (PsOp::Mad, Some(decode_operand(0)?), 3, None),
        D3DSIO_MUL => (PsOp::Mul, Some(decode_operand(0)?), 2, None),
        D3DSIO_RCP => (PsOp::Rcp, Some(decode_operand(0)?), 1, None),
        D3DSIO_RSQ => (PsOp::Rsq, Some(decode_operand(0)?), 1, None),
        D3DSIO_DP3 => (PsOp::Dp3, Some(decode_operand(0)?), 2, None),
        D3DSIO_DP4 => (PsOp::Dp4, Some(decode_operand(0)?), 2, None),
        D3DSIO_MIN => (PsOp::Min, Some(decode_operand(0)?), 2, None),
        D3DSIO_MAX => (PsOp::Max, Some(decode_operand(0)?), 2, None),
        D3DSIO_SLT => (PsOp::Slt, Some(decode_operand(0)?), 2, None),
        D3DSIO_SGE => (PsOp::Sge, Some(decode_operand(0)?), 2, None),
        D3DSIO_EXP => (PsOp::Exp, Some(decode_operand(0)?), 1, None),
        D3DSIO_LOG => (PsOp::Log, Some(decode_operand(0)?), 1, None),
        D3DSIO_LRP => (PsOp::Lrp, Some(decode_operand(0)?), 3, None),
        D3DSIO_FRC => (PsOp::Frc, Some(decode_operand(0)?), 1, None),
        D3DSIO_CMP => (PsOp::Cmp, Some(decode_operand(0)?), 3, None),
        // ── L5 vs_2_0 arithmetic ────────────────────────────────────────
        D3DSIO_M4x4 => (PsOp::M4x4, Some(decode_operand(0)?), 2, None),
        D3DSIO_M4x3 => (PsOp::M4x3, Some(decode_operand(0)?), 2, None),
        D3DSIO_M3x4 => (PsOp::M3x4, Some(decode_operand(0)?), 2, None),
        D3DSIO_M3x3 => (PsOp::M3x3, Some(decode_operand(0)?), 2, None),
        D3DSIO_M3x2 => (PsOp::M3x2, Some(decode_operand(0)?), 2, None),
        D3DSIO_DST => (PsOp::Dst, Some(decode_operand(0)?), 2, None),
        D3DSIO_LIT => (PsOp::Lit, Some(decode_operand(0)?), 1, None),
        D3DSIO_POW => (PsOp::Pow, Some(decode_operand(0)?), 2, None),
        D3DSIO_CRS => (PsOp::Crs, Some(decode_operand(0)?), 2, None),
        D3DSIO_SGN => (PsOp::Sgn, Some(decode_operand(0)?), 2, None),
        D3DSIO_ABS => (PsOp::Abs, Some(decode_operand(0)?), 1, None),
        D3DSIO_NRM => (PsOp::Nrm, Some(decode_operand(0)?), 1, None),
        D3DSIO_SINCOS => (PsOp::SinCos, Some(decode_operand(0)?), 3, None),
        D3DSIO_DP2ADD => (PsOp::Dp2Add, Some(decode_operand(0)?), 3, None),
        D3DSIO_MOVA => (PsOp::Mova, Some(decode_operand(0)?), 1, None),
        // ── L5 flow control ─────────────────────────────────────────────
        D3DSIO_IF => (PsOp::If, Some(decode_operand(0)?), 0, None),
        D3DSIO_IFC => (PsOp::Ifc, None, 2, None),
        D3DSIO_ELSE => (PsOp::Else, None, 0, None),
        D3DSIO_ENDIF => (PsOp::EndIf, None, 0, None),
        D3DSIO_LOOP => (PsOp::Loop, Some(decode_operand(0)?), 1, None),
        D3DSIO_ENDLOOP => (PsOp::EndLoop, None, 0, None),
        D3DSIO_REP => (PsOp::Rep, Some(decode_operand(0)?), 1, None),
        D3DSIO_ENDREP => (PsOp::EndRep, None, 0, None),
        D3DSIO_BREAK => (PsOp::Break, None, 0, None),
        D3DSIO_BREAKC => (PsOp::BreakC, None, 2, None),
        D3DSIO_BREAKP => (PsOp::BreakP, None, 1, None),
        D3DSIO_CALL => (PsOp::Call, None, 2, None),
        D3DSIO_CALLNZ => (PsOp::CallNz, None, 3, None),
        D3DSIO_LABEL => (PsOp::Label, Some(decode_operand(0)?), 0, None),
        D3DSIO_RET => (PsOp::Ret, None, 0, None),
        D3DSIO_SETP => (PsOp::Setp, Some(decode_operand(0)?), 2, None),
        D3DSIO_DEFB => {
            let dst = decode_operand(0)?;
            if dst.reg_type != RegType::ConstBool {
                anyhow::bail!("defb must target the boolean constant register file");
            }
            (PsOp::DefB, Some(dst), 0, None)
        }
        D3DSIO_DEFI => {
            let dst = decode_operand(0)?;
            if dst.reg_type != RegType::Loop {
                anyhow::bail!("defi must target the integer loop register file");
            }
            (PsOp::DefI, Some(dst), 0, None)
        }
        D3DSIO_TEX => {
            // ps_2_a/b texld forms: the opcode-specific control carries the
            // project (texldp) or bias (texldb) modifier.
            match control {
                0 => (PsOp::Tex, Some(decode_operand(0)?), 2, None),
                D3DSI_TEXLD_PROJECT => (PsOp::TexLdP, Some(decode_operand(0)?), 2, None),
                D3DSI_TEXLD_BIAS => (PsOp::TexLdB, Some(decode_operand(0)?), 2, None),
                other => anyhow::bail!("texld with unknown control 0x{other:x}"),
            }
        }
        D3DSIO_TEXKILL => (PsOp::TexKill, None, 1, None),
        D3DSIO_DEF => {
            let dst = decode_operand(0)?;
            if dst.reg_type != RegType::Const {
                anyhow::bail!("def must target the constant register file");
            }
            (PsOp::Def, Some(dst), 0, None)
        }
        D3DSIO_DCL => {
            let dst = decode_operand(0)?;
            // Sampler declarations carry the texture type (D3DSTT_2D = 2, …)
            // in the opcode-control bits 16-19; texcoord declarations ignore it.
            let tex_type = if dst.reg_type == RegType::Sampler {
                Some(control & 0x0F)
            } else {
                None
            };
            (PsOp::Dcl, Some(dst), 0, tex_type)
        }
        D3DSIO_END => (PsOp::End, None, 0, None),
        other => (PsOp::Unsupported(other), None, 0, None),
    };

    let mut srcs = Vec::with_capacity(src_count);
    // Source operands follow the destination operand (slot 0) when one
    // exists, so source `i` lives at payload index `i + 1` (+ the predicate
    // operand when present). A dst-less instruction (ifc/breakc/call/callnz)
    // starts its sources at slot 0.
    let src_base = usize::from(dst.is_some());
    for slot in src_base..src_base + src_count {
        srcs.push(decode_operand(slot)?);
    }
    let end = matches!(op, PsOp::End);
    Ok((
        PsInstruction {
            op,
            dst,
            srcs,
            tex_type,
            control: u8::try_from(control).unwrap_or(0),
            predicated,
            end,
        },
        total,
    ))
}

/// Tokenize a full shader bytecode stream.
///
/// Fails (`Err`) on a malformed stream: a non-2_x version token, an unknown
/// opcode, or an instruction whose operands run past the end of the stream.
/// `def cN, …` constants are collected into [`ParsedShader::constants`].
pub fn parse_shader(bytecode: &[u32]) -> Result<ParsedShader> {
    let version = *bytecode
        .first()
        .context("empty shader bytecode (missing version token)")?;
    let (kind, _major, _minor) = decode_shader_version(version)
        .with_context(|| format!("unsupported shader version token {version:#010x}"))?;

    let mut instructions = Vec::new();
    let mut constants = Vec::new();
    let mut bool_constants = Vec::new();
    let mut int_constants = Vec::new();
    let mut pos = 1_usize;
    let mut saw_end = false;
    while pos < bytecode.len() {
        let token = *bytecode
            .get(pos)
            .context("instruction token past end of bytecode")?;
        let opcode = token & OPCODE_FIELD_MASK;
        if opcode == D3DSIO_COMMENT {
            let payload =
                usize::try_from((token & COMMENTSIZE_FIELD_MASK) >> COMMENTSIZE_FIELD_SHIFT)
                    .context("comment size does not fit usize")?;
            pos = pos
                .checked_add(1)
                .and_then(|p| p.checked_add(payload))
                .context("comment payload past end of bytecode")?;
            continue;
        }
        let (instr, consumed) = parse_instruction(kind, bytecode.get(pos..).unwrap_or(&[]))?;
        if let PsOp::Def = instr.op {
            let dst = instr.dst.context("def instruction missing destination")?;
            // The four float DWORDs sit right after the operand token.
            let value = bytecode
                .get(
                    pos.checked_add(2)
                        .context("def float payload start overflow")?
                        ..pos
                            .checked_add(6)
                            .context("def float payload end overflow")?,
                )
                .context("def float payload missing")?
                .try_into()
                .map(|words: [u32; 4]| words.map(f32::from_bits))
                .context("def float payload is not 4 DWORDs")?;
            constants.push((u32::from(dst.reg_num), value));
        }
        if let PsOp::DefB = instr.op {
            let dst = instr.dst.context("defb instruction missing destination")?;
            // A defb carries one boolean DWORD (TRUE/FALSE) after the operand.
            let value = bytecode
                .get(pos.checked_add(2).context("defb payload start overflow")?)
                .copied()
                .context("defb boolean payload missing")?;
            bool_constants.push((u32::from(dst.reg_num), value != 0));
        }
        if let PsOp::DefI = instr.op {
            let dst = instr.dst.context("defi instruction missing destination")?;
            // A defi carries one signed-int DWORD after the operand.
            let value = bytecode
                .get(pos.checked_add(2).context("defi payload start overflow")?)
                .copied()
                .context("defi int payload missing")?;
            int_constants.push((
                u32::from(dst.reg_num),
                i32::from_le_bytes(value.to_le_bytes()),
            ));
        }
        saw_end = instr.end;
        instructions.push(instr);
        pos = pos
            .checked_add(consumed)
            .context("instruction stream position overflow")?;
        if saw_end {
            break;
        }
    }
    if !saw_end {
        anyhow::bail!("shader bytecode has no end token");
    }
    Ok(ParsedShader {
        kind,
        version,
        instructions,
        constants,
        bool_constants,
        int_constants,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Hand-assembled ps_2_0 bytecode (the same tokens the gui_d3d9 micro-exe
    /// ships): `def c0, 0,0,0,0` / `mov oC0, c0` / `end`.
    fn micro_exe_shader() -> Vec<u32> {
        vec![
            0xFFFF_0200, // ps_2_0 version
            0x0000_0051, // def (D3DSIO_DEF = 81)
            0x2000_0000, // c0 (CONST register file, regnum 0)
            0x0000_0000, // 0.0f
            0x0000_0000, // 0.0f
            0x0000_0000, // 0.0f
            0x0000_0000, // 0.0f
            0x0000_0001, // mov (D3DSIO_MOV = 1)
            0x000F_0800, // oC0 (COLOROUT = 8: regtype bit 3 in bit 11; writemask all)
            0x20E4_0000, // c0 source with NOSWIZZLE (D3DSP_NOSWIZZLE = 0xE4 << 16)
            0x0000_FFFF, // end
        ]
    }

    #[test]
    fn tokenizes_micro_exe_shader() {
        let shader = parse_shader(&micro_exe_shader()).expect("micro-exe shader parses");
        assert_eq!(shader.kind, ShaderKind::Pixel);
        assert_eq!(shader.version, 0xFFFF_0200);
        assert!(shader.is_fully_executable());
        assert_eq!(shader.instructions.len(), 3);

        let def = shader
            .instructions
            .first()
            .expect("shader starts with a def");
        assert_eq!(def.op, PsOp::Def);
        assert_eq!(shader.constants, vec![(0, [0.0; 4])]);

        let mov = shader.instructions.get(1).expect("shader has a mov");
        assert_eq!(mov.op, PsOp::Mov);
        let dst = mov.dst.expect("mov has a destination");
        assert_eq!(dst.reg_type, RegType::ColorOut);
        assert_eq!(dst.reg_num, 0);
        assert_eq!(dst.write_mask, 0xF);
        assert_eq!(dst.dst_mod, D3DSPDM_NONE);
        let src = mov.srcs.first().expect("mov has one source");
        assert_eq!(src.reg_type, RegType::Const);
        assert_eq!(src.reg_num, 0);
        assert_eq!(src.swizzle, [0, 1, 2, 3]);
        assert_eq!(src.src_mod, D3DSPSM_NONE);

        let end = shader.instructions.get(2).expect("shader ends with an end");
        assert_eq!(end.op, PsOp::End);
        assert!(end.end);
    }

    #[test]
    fn tokenizer_decodes_version_family_and_rejects_others() {
        // ps_2_a / ps_2_b / ps_2_sw are the ps_2_0 family.
        for version in [
            0xFFFF_0200,
            0xFFFF_0201,
            0xFFFF_0202,
            0xFFFF_02FE,
            0xFFFE_0200,
            0xFFFE_0201,
        ] {
            assert!(
                decode_shader_version(version).is_some(),
                "version {version:#x}"
            );
        }
        for version in [
            0xFFFF_0300,
            0xFFFE_0100,
            0xFFFF_0104,
            0x0000_0200,
            0xFFFF_0203,
        ] {
            assert!(
                decode_shader_version(version).is_none(),
                "version {version:#x}"
            );
        }
    }

    #[test]
    fn tokenizer_round_trips_operand_fields() {
        // A source operand with a NEG modifier, .ywzw swizzle and const reg.
        let token = 0x2000_0000 // CONST c0
            | u32::from(D3DSPSM_NEG) << D3DSP_SRCMOD_SHIFT
            | (0x01u32 << 16)  // out.x <- in.y
            | (0x03u32 << (16 + 2))  // out.y <- in.w
            | (0x02u32 << (16 + 4))  // out.z <- in.z
            | (0x03u32 << (16 + 6)); // out.w <- in.w
        let op = parse_operand(token);
        assert_eq!(op.reg_type, RegType::Const);
        assert_eq!(op.reg_num, 0);
        assert_eq!(op.src_mod, D3DSPSM_NEG);
        assert_eq!(op.swizzle, [1, 3, 2, 3]);
    }

    #[test]
    fn tokenizer_decodes_saturate_destination_modifier() {
        // dst operand: TEMP r0 with SATURATE + .xy write mask.
        let token = u32::from(D3DSPDM_SATURATE) << D3DSP_DSTMOD_SHIFT | 0x3 << 16;
        let op = parse_operand(token);
        assert_eq!(op.reg_type, RegType::Temp);
        assert_eq!(op.dst_mod, D3DSPDM_SATURATE);
        assert_eq!(op.write_mask, 0x3);
    }

    #[test]
    fn tokenizer_parses_dcl_and_tex() {
        // ps_2_0: dcl t0 / dcl_2d s0 / texld r0, t0, s0 / end
        let bytecode = vec![
            0xFFFF_0200, // ps_2_0
            0x0200_001F, // dcl, length-2 payload (1 operand), control 0
            0x3000_0000, // t0 (TEXTURE = 3)
            0x0202_001F, // dcl, control = D3DSTT_2D (2)
            0x2000_0800, // s0 (SAMPLER = 10: low 3 bits 2 + bit 11)
            0x0200_0042, // texld (D3DSIO_TEX = 66)
            0x0000_0000, // r0 (TEMP = 0)
            0x3000_0000, // t0
            0x2000_0800, // s0
            0x0000_FFFF, // end
        ];
        let shader = parse_shader(&bytecode).expect("dcl/tex shader parses");
        assert!(shader.is_fully_executable());
        let dcl_t = shader
            .instructions
            .first()
            .expect("shader starts with dcl t0");
        assert_eq!(dcl_t.op, PsOp::Dcl);
        assert_eq!(
            dcl_t.dst.expect("dcl has operand").reg_type,
            RegType::Texture
        );
        assert_eq!(dcl_t.tex_type, None);
        let dcl_s = shader.instructions.get(1).expect("shader has dcl s0");
        assert_eq!(
            dcl_s.dst.expect("dcl has operand").reg_type,
            RegType::Sampler
        );
        assert_eq!(dcl_s.tex_type, Some(2));
        let tex = shader.instructions.get(2).expect("shader has texld");
        assert_eq!(tex.op, PsOp::Tex);
        assert_eq!(tex.srcs.len(), 2);
        assert_eq!(
            tex.srcs.first().expect("texld has a uv source").reg_type,
            RegType::Texture
        );
        assert_eq!(
            tex.srcs
                .get(1)
                .expect("texld has a sampler source")
                .reg_type,
            RegType::Sampler
        );
    }

    #[test]
    fn tokenizer_rejects_unsupported_version() {
        let mut bytecode = micro_exe_shader();
        *bytecode.first_mut().expect("bytecode has a version token") = 0xFFFF_0300; // ps_3_0 — unsupported
        assert!(parse_shader(&bytecode).is_err());
    }

    #[test]
    fn tokenizer_rejects_missing_end() {
        let mut bytecode = micro_exe_shader();
        bytecode.pop(); // drop the end token
        assert!(parse_shader(&bytecode).is_err());
    }

    #[test]
    fn tokenizer_rejects_truncated_operand() {
        let mut bytecode = micro_exe_shader();
        bytecode.truncate(8); // mov's source operand is gone
        assert!(parse_shader(&bytecode).is_err());
    }

    #[test]
    fn tokenizer_skips_comment_tokens() {
        let bytecode = vec![
            0xFFFF_0200, // ps_2_0
            0x0003_FFFE, // comment, 3 payload DWORDs (D3DSI_COMMENTSIZE | D3DSIO_COMMENT)
            0xDEAD_BEEF,
            0xCAFE_F00D,
            0x1234_5678,
            0x0000_0001, // mov
            0x000F_0800, // oC0
            0x20E4_0000, // c0
            0x0000_FFFF, // end
        ];
        let shader = parse_shader(&bytecode).expect("comment tokens skipped");
        assert_eq!(shader.instructions.len(), 2);
        assert_eq!(
            shader
                .instructions
                .first()
                .expect("shader starts with mov")
                .op,
            PsOp::Mov
        );
    }

    #[test]
    fn unsupported_opcode_parses_but_is_not_executable() {
        let bytecode = vec![
            0xFFFF_0200, // ps_2_0
            0x0000_0043, // texbem (D3DSIO_TEXBEM = 67) — valid ps_1_x, exec deferred
            0x0000_0000, // r0 (its single payload operand per the length table)
            0x0000_FFFF, // end
        ];
        let shader = parse_shader(&bytecode).expect("texbem parses structurally");
        assert_eq!(
            shader
                .instructions
                .first()
                .expect("shader starts with texbem")
                .op,
            PsOp::Unsupported(67)
        );
        assert!(!shader.is_fully_executable());
    }

    /// vs_2_0 bytecode exercising the L5 surface: `defb`/`defi`, a relative
    /// read `c[a0.x + 8]`, a predicated `mov`, a `loop` with a `breakc`, and
    /// an `if/else/endif` — every decoded form the gate must now accept.
    fn l5_vs_bytecode() -> Vec<u32> {
        vec![
            0xFFFE_0200, // vs_2_0
            0x0000_002F, // defb
            0x6000_0800, // b0 (CONSTBOOL)
            0x0000_0001, // TRUE
            0x0000_0030, // defi
            0x7000_0800, // i0 (LOOP)
            0x0000_0002, // 2
            0x0000_001B, // loop
            0x7000_0800, // i0 (LOOP dst)
            0x70E4_0801, // i1 (LOOP src, NOSWIZZLE) — the (aL,aU,aD,aC) spec
            0x1000_0001, // mov, predicated (bit 28)
            0x3000_1000, // p0 (PREDICATE)
            0x0000_0000, // r0 (TEMP dst)
            0x20E4_2004, // c[a0.x + 4] (CONST regnum 4, bit 13 relative, NOSWIZZLE)
            0x0003_002D, // breakc, comparison GE (3)
            0x70E4_0800, // i0
            0x70E4_0801, // i1
            0x0000_001D, // endloop
            0x0000_0028, // if
            0x60E4_0800, // b0
            0x0000_002A, // else
            0x0000_002B, // endif
            0x0000_FFFF, // end
        ]
    }

    #[test]
    fn tokenizer_decodes_l5_forms() {
        let bytes = l5_vs_bytecode();
        let shader = match parse_shader(&bytes) {
            Ok(shader) => shader,
            Err(err) => {
                tracing::error!("parse error: {err:?}");
                panic!("L5 vs bytecode parses: {err}");
            }
        };
        assert_eq!(shader.kind, ShaderKind::Vertex);
        assert!(
            shader.is_fully_executable(),
            "every L5 op must pass the gate"
        );

        // defb b0, TRUE / defi i0, 2 land in the constant tables.
        assert_eq!(shader.bool_constants, vec![(0, true)]);
        assert_eq!(shader.int_constants, vec![(0, 2)]);

        let ops: Vec<PsOp> = shader.instructions.iter().map(|instr| instr.op).collect();
        assert_eq!(
            ops,
            vec![
                PsOp::DefB,
                PsOp::DefI,
                PsOp::Loop,
                PsOp::Mov,
                PsOp::BreakC,
                PsOp::EndLoop,
                PsOp::If,
                PsOp::Else,
                PsOp::EndIf,
                PsOp::End,
            ]
        );

        // The relative read decodes bit 13 on the c4 source.
        let mov = shader
            .instructions
            .get(3)
            .expect("shader has the predicated mov");
        assert!(mov.predicated, "the mov must carry the predicated flag");
        let src = mov.srcs.first().expect("mov has a source");
        assert!(src.relative, "c[a0.x + 4] must decode as relative");
        assert_eq!(src.reg_type, RegType::Const);
        assert_eq!(src.reg_num, 4);

        // The loop's destination is the i0 loop register; the breakc carries
        // the GE comparison control (D3DSPC_GE = 3).
        let brk = shader.instructions.get(4).expect("shader has breakc");
        assert_eq!(brk.control, D3DSPC_GE);
        assert_eq!(brk.srcs.len(), 2);
    }

    #[test]
    fn tokenizer_decodes_ps_2a_texld_forms() {
        // ps_2_a: texldp r0, t0, s0 (project control 0x1) + texldb (0x2).
        let bytecode = vec![
            0xFFFF_0201, // ps_2_a
            0x0201_0042, // texld with the project control (0x1 << 16)
            0x0000_0000, // r0
            0x30E4_0000, // t0
            0x2000_0800, // s0
            0x0202_0042, // texld with the bias control (0x2 << 16)
            0x0000_0000, // r0
            0x30E4_0000, // t0
            0x2000_0800, // s0
            0x0000_FFFF, // end
        ];
        let shader = parse_shader(&bytecode).expect("ps_2_a texld forms parse");
        assert_eq!(shader.kind, ShaderKind::Pixel);
        assert!(shader.is_fully_executable());
        let ops: Vec<PsOp> = shader.instructions.iter().map(|instr| instr.op).collect();
        assert_eq!(ops, vec![PsOp::TexLdP, PsOp::TexLdB, PsOp::End]);
    }

    #[test]
    fn tokenizer_decodes_relative_and_predicate_operands() {
        // A relative source (bit 13) on the CONST file.
        let token = 0x20E4_2000 | 0x0000_0007; // c[a0.x + 7]
        let op = parse_operand(token);
        assert!(op.relative);
        assert_eq!(op.reg_type, RegType::Const);
        assert_eq!(op.reg_num, 7);
        // A predicate register operand (PREDICATE = 19).
        let pred = parse_operand(0x3000_1000);
        assert_eq!(pred.reg_type, RegType::Predicate);
        // A label operand (LABEL = 18).
        let label = parse_operand(0x2000_1000);
        assert_eq!(label.reg_type, RegType::Label);
    }

    #[test]
    fn tokenizer_accepts_flow_control_payload_counts() {
        // ifc with the GE comparison and two sources (3 operands total).
        let bytecode = vec![
            0xFFFE_0200, // vs_2_0
            0x0003_0029, // ifc, comparison GE (3)
            0x70E4_0800, // i0
            0x70E4_0801, // i1
            0x0000_0028, // if
            0x60E4_0800, // b0
            0x0000_002B, // endif
            0x0000_FFFF, // end
        ];
        let shader = parse_shader(&bytecode).expect("ifc/if parse");
        assert!(shader.is_fully_executable());
        let ifc = shader.instructions.first().expect("shader starts with ifc");
        assert_eq!(ifc.op, PsOp::Ifc);
        assert_eq!(ifc.control, D3DSPC_GE);
        assert_eq!(ifc.srcs.len(), 2);
    }
}
