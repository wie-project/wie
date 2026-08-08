//! Shared shader flow-control machinery (the ps_2_x / vs_2_0 block forms).
//!
//! Both interpreters execute a flat, tokenized instruction list with an
//! explicit program counter. This module owns the two things they share:
//!
//! - [`FlowMap`]: the precomputed jump targets for every structured block
//!   (`if`/`ifc`/`else`/`endif`, `loop`/`endloop`, `rep`/`endrep`), built in
//!   one stack pass so branch execution is O(1) per instruction.
//! - [`FlowState`]: the runtime stacks (the call stack for `call`/`ret`, the
//!   nested block stack for `loop`/`rep`/`break`) plus the step budget that
//!   bounds a hostile or malformed shader's total work.
//!
//! The comparison helper (`ifc` / `breakc` / `setp` opcode-control
//! comparisons, `D3DSPC_*`) lives here too — it is identical in both stages.

use crate::d3d9_shader::{PsInstruction, PsOp};
use ahash::HashMapExt;

/// The vs_2_0 subroutine call-stack depth limit (`call`/`callnz` nesting).
pub(super) const CALL_DEPTH_LIMIT: usize = 32;
/// Instruction-step budget per shader execution.
///
/// A guest shader may `loop`/`rep` with an unbounded iteration count; the
/// interpreter stops once this many instructions have executed (the outputs
/// accumulated so far are returned). Bounds a hostile bytecode's CPU cost.
pub(super) const MAX_SHADER_STEPS: u32 = 1_000_000;

/// One open `loop`/`rep` block during execution.
#[derive(Debug)]
pub(super) enum BlockState {
    /// An open `loop iN, cI`: `value` is the current counter (`iN.x`),
    /// `remaining` the iterations left (initially `aU`), `step` the per-
    /// iteration increment (`aD`).
    Loop {
        /// The loop counter register (`i0..i3`) holding `value`.
        reg: u16,
        /// The current counter value (`iN.x`).
        value: i32,
        /// Iterations left before the loop exits.
        remaining: i32,
        /// The per-iteration step (`aD`).
        step: i32,
        /// The `loop` instruction's pc (the body starts at `start + 1`).
        start: usize,
        /// The matching `endloop` pc (a `break` jumps to `end + 1`).
        end: usize,
    },
    /// An open `rep iN, cI`: the body repeats `remaining` more times.
    Rep {
        /// Iterations left before the repeat exits.
        remaining: i32,
        /// The `rep` instruction's pc (the body starts at `start + 1`).
        start: usize,
        /// The matching `endrep` pc (a `break` jumps to `end + 1`).
        end: usize,
    },
}

/// The per-execution control-flow state (stacks + step budget).
#[derive(Debug, Default)]
pub(super) struct FlowState {
    /// Return addresses for `call`/`ret` (bounded by [`CALL_DEPTH_LIMIT`]).
    pub(super) call_stack: Vec<usize>,
    /// Open `loop`/`rep` blocks, innermost last (`break` pops the last).
    pub(super) blocks: Vec<BlockState>,
    /// Instructions executed so far (bounded by [`MAX_SHADER_STEPS`]).
    pub(super) steps: u32,
}

/// Precomputed control-flow jump targets for one shader.
#[derive(Debug, Default)]
pub(super) struct FlowMap {
    /// `if`/`ifc` pc → the matching `else` pc (or the `endif` when no else).
    branch_else: Vec<Option<usize>>,
    /// `else` pc → the matching `endif` pc.
    else_endif: Vec<Option<usize>>,
    /// `loop` pc → the matching `endloop` pc.
    loop_end: Vec<Option<usize>>,
    /// `rep` pc → the matching `endrep` pc.
    rep_end: Vec<Option<usize>>,
    /// `label l#` regnum → the label instruction's pc (`call` targets).
    label_pc: ahash::HashMap<u16, usize>,
}

impl FlowMap {
    /// Build the jump map with one stack pass over the instruction list.
    ///
    /// Shader block structure is well-nested by construction (a compiler
    /// emitted it); a malformed stream leaves the unmatched entries `None`
    /// and the interpreters treat those as "end the shader".
    #[must_use]
    pub(super) fn build(instructions: &[PsInstruction]) -> Self {
        let len = instructions.len();
        let mut map = FlowMap {
            branch_else: vec![None; len],
            else_endif: vec![None; len],
            loop_end: vec![None; len],
            rep_end: vec![None; len],
            label_pc: ahash::HashMap::new(),
        };
        // A branch entry tracks (if_pc, else_pc) so an `endif` can resolve
        // both the if's target and the else's target.
        let mut branches: Vec<(usize, Option<usize>)> = Vec::new();
        let mut loops: Vec<usize> = Vec::new();
        let mut reps: Vec<usize> = Vec::new();
        for (pc, instr) in instructions.iter().enumerate() {
            match instr.op {
                PsOp::If | PsOp::Ifc => branches.push((pc, None)),
                PsOp::Else => {
                    if let Some(entry) = branches.last_mut()
                        && entry.1.is_none()
                    {
                        entry.1 = Some(pc);
                        // A false `if` jumps past the `else` marker to the
                        // else BODY's first instruction (`else` itself, when
                        // reached by the true path's fall-through, jumps to
                        // the endif — so the two paths never collide).
                        if let Some(slot) = map.branch_else.get_mut(entry.0) {
                            *slot = Some(pc.saturating_add(1));
                        }
                    }
                }
                PsOp::EndIf => {
                    if let Some((if_pc, else_pc)) = branches.pop() {
                        // No else → the if jumps straight here.
                        if map.branch_else.get(if_pc).and_then(|t| *t).is_none()
                            && let Some(slot) = map.branch_else.get_mut(if_pc)
                        {
                            *slot = Some(pc);
                        }
                        if let Some(else_pc) = else_pc
                            && let Some(slot) = map.else_endif.get_mut(else_pc)
                        {
                            *slot = Some(pc);
                        }
                    }
                }
                PsOp::Loop => loops.push(pc),
                PsOp::EndLoop => {
                    if let Some(loop_pc) = loops.pop()
                        && let Some(slot) = map.loop_end.get_mut(loop_pc)
                    {
                        *slot = Some(pc);
                    }
                }
                PsOp::Rep => reps.push(pc),
                PsOp::EndRep => {
                    if let Some(rep_pc) = reps.pop()
                        && let Some(slot) = map.rep_end.get_mut(rep_pc)
                    {
                        *slot = Some(pc);
                    }
                }
                PsOp::Label => {
                    if let Some(dst) = &instr.dst {
                        map.label_pc.insert(dst.reg_num, pc);
                    }
                }
                _ => {}
            }
        }
        map
    }

    /// The target of a taken `if`/`ifc`: the `else` when present, else `endif`.
    pub(super) fn branch_target(&self, pc: usize) -> Option<usize> {
        self.branch_else.get(pc).copied().flatten()
    }

    /// The `endif` a fall-through `else` must jump to.
    pub(super) fn else_target(&self, pc: usize) -> Option<usize> {
        self.else_endif.get(pc).copied().flatten()
    }

    /// The `endloop` matching a `loop`.
    pub(super) fn loop_end(&self, pc: usize) -> Option<usize> {
        self.loop_end.get(pc).copied().flatten()
    }

    /// The `endrep` matching a `rep`.
    pub(super) fn rep_end(&self, pc: usize) -> Option<usize> {
        self.rep_end.get(pc).copied().flatten()
    }

    /// The pc of `label l#`, or `None` when the label is undefined.
    pub(super) fn label(&self, reg_num: u16) -> Option<usize> {
        self.label_pc.get(&reg_num).copied()
    }
}

/// The `D3DSPC_*` comparison over two scalars (`ifc` / `breakc` / `setp`).
#[must_use]
pub(super) fn compare(a: f32, b: f32, cmp: u8) -> bool {
    match cmp {
        crate::d3d9_shader::D3DSPC_GT => a > b,
        crate::d3d9_shader::D3DSPC_EQ => a == b,
        crate::d3d9_shader::D3DSPC_GE => a >= b,
        crate::d3d9_shader::D3DSPC_LT => a < b,
        crate::d3d9_shader::D3DSPC_NE => a != b,
        crate::d3d9_shader::D3DSPC_LE => a <= b,
        // RESERVED0/RESERVED1 never compare true.
        _ => false,
    }
}
