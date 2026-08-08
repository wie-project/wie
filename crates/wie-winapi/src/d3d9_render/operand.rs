//! Shared shader operand machinery for the PS and VS 2.0 interpreters.
//!
//! Both stages execute the same D3D9 operand semantics — source modifiers
//! (`D3DSPSM_*`), swizzles, write masks and the saturate destination
//! modifier — against different register files ([`PsRegisters`] keeps the
//! `tN` texcoords, `oC0` and a flat 4-wide integer file;
//! [`VsRegisters`](super::vs::VsRegisters) keeps the `a0` address register,
//! the bool/int constant files, relative addressing and the rasterizer output
//! files). This module owns the shared part: [`ShaderRegisters`] abstracts the
//! register file (fetch/slot), and the operand readers/writers are generic
//! over it. The register-type → storage mapping (which file a `RegType`
//! names) is the only per-stage code, so the shared operand semantics cannot
//! drift between the stages. [`FlowMap`](super::flow::FlowMap) is the same
//! pattern for the shared flow-control machinery.

use crate::d3d9_shader::{
    D3DSP_WRITEMASK_0, D3DSP_WRITEMASK_1, D3DSP_WRITEMASK_2, D3DSP_WRITEMASK_3, D3DSPDM_SATURATE,
    D3DSPSM_ABS, D3DSPSM_ABSNEG, D3DSPSM_BIAS, D3DSPSM_BIASNEG, D3DSPSM_COMP, D3DSPSM_NEG,
    D3DSPSM_SIGN, D3DSPSM_SIGNNEG, D3DSPSM_X2, D3DSPSM_X2NEG, Operand, RegType,
};

/// The register-file interface both interpreters execute against.
///
/// `fetch` / `slot` are the per-stage halves (the register-type → storage
/// mapping); everything else in this module is shared and generic over this
/// trait.
pub(super) trait ShaderRegisters {
    /// Fetch a register file value; out-of-range indices read zero.
    #[must_use]
    fn fetch(&self, reg_type: RegType, index: usize) -> [f32; 4];

    /// Fetch the raw integer value of a `RegType::Loop` register (the
    /// `loop`/`rep` counters read the stored ints, not a float truncation).
    #[must_use]
    fn fetch_i32(&self, index: usize) -> [i32; 4];

    /// The effective register-file index for a source operand: the register
    /// number plus the integer part of `a0.x` when the operand is relative
    /// (`c[a0.x + n]`). The pixel stage has no relative forms.
    #[must_use]
    fn effective_index(&self, op: &Operand) -> usize;

    /// The mutable destination slot for a write, or `None` when the register
    /// type is not a valid destination for this stage (the write is dropped).
    fn slot(&mut self, reg_type: RegType, index: usize) -> Option<&mut [f32; 4]>;

    /// Merge `value` into the destination with the write mask (bit 0 = x,
    /// bit 1 = y, bit 2 = z, bit 3 = w); components the mask clears keep
    /// their current value.
    fn store(&mut self, reg_type: RegType, index: usize, mask: u8, value: [f32; 4]) {
        let Some(target) = self.slot(reg_type, index) else {
            return;
        };
        let [x, y, z, w] = value;
        let [ox, oy, oz, ow] = *target;
        *target = [
            if mask & D3DSP_WRITEMASK_0 != 0 { x } else { ox },
            if mask & D3DSP_WRITEMASK_1 != 0 { y } else { oy },
            if mask & D3DSP_WRITEMASK_2 != 0 { z } else { oz },
            if mask & D3DSP_WRITEMASK_3 != 0 { w } else { ow },
        ];
    }
}

/// Read the `i`-th component without indexing syntax (repo lint).
#[inline]
#[must_use]
pub(super) fn comp(value: [f32; 4], i: usize) -> f32 {
    value.get(i).copied().unwrap_or(0.0)
}

/// Read the `i`-th component of a raw integer value.
#[must_use]
pub(super) fn comp_opt(value: [i32; 4], i: usize) -> i32 {
    value.get(i).copied().unwrap_or(0)
}

/// Apply a source modifier (`D3DSPSM_*`) to a register value.
///
/// `DZ`/`DW` (texcoord-depth modifiers) and `NOT` (boolean registers) are
/// unmodeled and read as identity — they do not occur in the ps_2_0 / vs_2_0
/// subsets the interpreters execute.
#[must_use]
pub(super) fn apply_src_mod(value: [f32; 4], src_mod: u8) -> [f32; 4] {
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
pub(super) fn apply_swizzle(value: [f32; 4], swizzle: [u8; 4]) -> [f32; 4] {
    [
        comp(value, usize::from(swizzle[0])),
        comp(value, usize::from(swizzle[1])),
        comp(value, usize::from(swizzle[2])),
        comp(value, usize::from(swizzle[3])),
    ]
}

/// Read a source operand: register fetch → source modifier → swizzle.
#[must_use]
pub(super) fn read_operand<R: ShaderRegisters>(regs: &R, op: &Operand) -> [f32; 4] {
    let base = regs.fetch(op.reg_type, regs.effective_index(op));
    apply_swizzle(apply_src_mod(base, op.src_mod), op.swizzle)
}

/// Read a source operand's register value as raw integers.
///
/// Integer registers (`iN`) read the stored ints (swizzled); any other file
/// falls back to truncating the float read (the `loop`/`breakc` integer
/// sources).
#[must_use]
pub(super) fn read_operand_i32<R: ShaderRegisters>(regs: &R, op: &Operand) -> [i32; 4] {
    let index = regs.effective_index(op);
    if op.reg_type == RegType::Loop {
        let raw = regs.fetch_i32(index);
        let sw = op.swizzle;
        [
            comp_opt(raw, usize::from(sw[0])),
            comp_opt(raw, usize::from(sw[1])),
            comp_opt(raw, usize::from(sw[2])),
            comp_opt(raw, usize::from(sw[3])),
        ]
    } else {
        let value = read_operand(regs, op);
        [
            comp(value, 0).trunc() as i32,
            comp(value, 1).trunc() as i32,
            comp(value, 2).trunc() as i32,
            comp(value, 3).trunc() as i32,
        ]
    }
}

/// Write an operand's value into the register file (write mask + saturate).
///
/// `_sat` clamps the written components to `[0, 1]`; `_pp` (partial
/// precision) is ignored (full f32 precision, documented). Which register
/// types accept writes is the stage's call — the vertex stage stores the
/// rasterizer/attribute/texcoord outputs, `a0` and `p0`; the pixel stage
/// stores `oC0` and `p0`; everything else (`oDepth`, the constant/bool/input
/// files, …) is dropped.
pub(super) fn write_operand<R: ShaderRegisters>(regs: &mut R, op: &Operand, value: [f32; 4]) {
    let mut result = value;
    if op.dst_mod == D3DSPDM_SATURATE {
        for channel in &mut result {
            *channel = channel.clamp(0.0, 1.0);
        }
    }
    regs.store(op.reg_type, usize::from(op.reg_num), op.write_mask, result);
}

/// Read exactly two source operands; short source lists read as zero.
#[must_use]
pub(super) fn read_two<R: ShaderRegisters>(regs: &R, srcs: &[Operand]) -> [[f32; 4]; 2] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b]
}

/// Read exactly three source operands; short source lists read as zero.
#[must_use]
pub(super) fn read_three<R: ShaderRegisters>(regs: &R, srcs: &[Operand]) -> [[f32; 4]; 3] {
    let a = srcs.first().map_or([0.0; 4], |op| read_operand(regs, op));
    let b = srcs.get(1).map_or([0.0; 4], |op| read_operand(regs, op));
    let c = srcs.get(2).map_or([0.0; 4], |op| read_operand(regs, op));
    [a, b, c]
}
