//! Decode a lowerable straight-line block for Cranelift (GPR + simple mem + jcc/jmp term).

use super::lower::err::LowerError;
use super::lower::opclass::{self, TermForm};
use crate::exec::HookWindow;
use crate::mem::GuestMemory;
use iced_x86::{Decoder, DecoderOptions, Instruction, MemorySize, Mnemonic, OpKind, Register};

/// Max instructions per compiled block (keeps compile time bounded).
/// Raised to capture longer pure loop bodies in one native frame.
pub(super) const MAX_BLOCK_INSNS: usize = 96;
/// Min instructions before paying Cranelift compile cost (short blocks lose wall).
/// 2 keeps tiny fallthrough fragments eligible once hot (was 4; 7za residual was
/// dominated by already-lowerable Mov/Add fragments that never reached min=4).
pub(super) const MIN_BLOCK_INSNS: usize = 2;

/// One decoded guest insn kept for the lowerer.
#[derive(Debug, Clone)]
pub(super) struct DecodedInsn {
    pub instr: Instruction,
}

/// Block exit control-flow (last insn of a Pure block, if any).
#[derive(Debug, Clone, Copy)]
pub(super) enum BlockTerm {
    /// Unconditional near jump.
    Jmp { target: u64 },
    /// Conditional near jump; fallthrough is `not_taken`.
    Jcc {
        mnemonic: Mnemonic,
        taken: u64,
        not_taken: u64,
    },
    /// Near `call rel` — push `return_ip`, exit at `target`.
    Call { target: u64, return_ip: u64 },
    /// Near `ret` (no imm) — pop exit RIP.
    Ret,
}

/// Result of trying to form a JIT block at `start`.
///
/// `Clone` so the background compiler can be handed the *exact* block a guest
/// thread classified (background install must compile the same bytes, not a
/// later re-decode that could observe different guest memory).
#[derive(Clone)]
pub(super) enum BlockKind {
    /// Lowerable body (+ optional terminator) ending at `end_rip` when no term / fallthrough.
    Pure {
        insns: Vec<DecodedInsn>,
        /// Fallthrough RIP when the block has no terminator, or jcc not-taken.
        /// For unconditional `jmp`, equals the jump target.
        end_rip: u64,
        bytes_len: u32,
        term: Option<BlockTerm>,
    },
    /// Needs interpreter (complex mem / call / ret / sse / …).
    NotPure,
}

/// Decode from guest memory until a non-lowerable insn, terminator, or max length.
///
/// A near `jcc`/`jmp` terminator is **included** and ends the block. Other
/// non-lowerable insns are **not** included (interpreter runs them next).
pub(super) fn decode_pure_gpr_block(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    start: u64,
) -> BlockKind {
    // Small initial capacity: typical blocks are 2–10 insns, so reserving
    // `MAX_BLOCK_INSNS` (96) slots per compile wastes a 96-element allocation.
    // Growth is geometric when a block really is long; the loop bound below
    // still enforces `MAX_BLOCK_INSNS` exactly.
    let mut insns = Vec::with_capacity(16);
    let mut rip = start;
    let mut bytes_len = 0_u32;
    let mut term: Option<BlockTerm> = None;

    if let Some(h) = hooks
        && h.should_host_stop(start)
    {
        return BlockKind::NotPure;
    }

    for _ in 0..MAX_BLOCK_INSNS {
        let mut buf = [0_u8; 15];
        if mem.read(rip, &mut buf).is_err() {
            break;
        }
        let mut decoder = Decoder::with_ip(64, &buf, rip, DecoderOptions::NONE);
        let instr = decoder.decode();
        if instr.is_invalid() || instr.len() == 0 {
            break;
        }
        let len_u32 = u32::try_from(instr.len()).unwrap_or(0);
        let next = instr.next_ip();

        if let Some(t) = classify_terminator(&instr) {
            // Unconditional jmp to the very next instruction: treat as no-op
            // and keep decoding to merge the fallthrough block into this one.
            if matches!(t, BlockTerm::Jmp { target } if target == next) {
                rip = next;
                bytes_len = bytes_len.saturating_add(len_u32);
                continue;
            }
            insns.push(DecodedInsn { instr });
            bytes_len = bytes_len.saturating_add(len_u32);
            term = Some(t);
            rip = match t {
                BlockTerm::Jmp { target } | BlockTerm::Call { target, .. } => target,
                BlockTerm::Jcc { not_taken, .. } => not_taken,
                // Dynamic; placeholder for decode end only (not used as fallthrough).
                BlockTerm::Ret => next,
            };
            break;
        }

        if !is_lowerable(&instr) {
            break;
        }
        let ends_string = opclass::is_string_op(&instr);
        insns.push(DecodedInsn { instr });
        bytes_len = bytes_len.saturating_add(len_u32);
        rip = next;
        // String ops end the block: REP stay needs dynamic exit RIP from the host helper.
        if ends_string {
            break;
        }
    }

    // Short blocks with a terminator are still worth compiling:
    // - call/ret: UCRT fast-path + shadow-stack chaining beats host-stop
    // - jcc/jmp: tight loops (often < MIN_BLOCK_INSNS) must not stay on iced
    // - string ops: bulk REP helper
    // Fallthrough-only fragments keep MIN_BLOCK_INSNS to avoid compile tax.
    let min = if term.is_some()
        || insns
            .last()
            .is_some_and(|d| opclass::is_string_op(&d.instr))
    {
        1
    } else {
        MIN_BLOCK_INSNS
    };
    if insns.len() < min {
        BlockKind::NotPure
    } else {
        BlockKind::Pure {
            insns,
            end_rip: rip,
            bytes_len,
            term,
        }
    }
}

/// True when a Pure block's terminator is a self-loop back to `start`.
#[must_use]
pub(super) fn pure_is_self_loop(kind: &BlockKind, start: u64) -> bool {
    match kind {
        BlockKind::Pure { term, .. } => term_is_self_loop(term.as_ref(), start),
        _ => false,
    }
}

/// Whether a decoded block's terminator branches back to the block's own
/// entry. Split out of [`pure_is_self_loop`] because the tier-up policy
/// ([`crate::jit::tier`]) asks the same question of an already-destructured
/// `BlockKind`, and one definition of "self-loop" must back both.
#[must_use]
pub(super) fn term_is_self_loop(term: Option<&BlockTerm>, start: u64) -> bool {
    match term {
        Some(BlockTerm::Jmp { target }) => *target == start,
        Some(BlockTerm::Jcc {
            taken, not_taken, ..
        }) => *taken == start || *not_taken == start,
        _ => false,
    }
}

fn classify_terminator(instr: &Instruction) -> Option<BlockTerm> {
    match opclass::term_of(instr)? {
        TermForm::Ret => Some(BlockTerm::Ret),
        TermForm::Call => Some(BlockTerm::Call {
            target: instr.near_branch_target(),
            return_ip: instr.next_ip(),
        }),
        TermForm::Jmp => Some(BlockTerm::Jmp {
            target: instr.near_branch_target(),
        }),
        TermForm::Jcc => Some(BlockTerm::Jcc {
            mnemonic: instr.mnemonic(),
            taken: instr.near_branch_target(),
            not_taken: instr.next_ip(),
        }),
    }
}

/// May this instruction be compiled into a block body?
///
/// The answer comes from the operation-class table
/// ([`crate::jit::lower::opclass`]), which is the same table `lower_insn`
/// consults when it refuses a mnemonic and the same one the liveness analysis
/// reads for implicit operands. One table for all three questions is the point
/// of ADR-0007: the gate and the lowering can no longer disagree about *which*
/// mnemonics exist, and an operand form is decided once instead of once per
/// consumer.
fn is_lowerable(instr: &Instruction) -> bool {
    opclass::admits(instr)
}

/// Pre-compile plan: every memop in the block is `base+disp` on one stack
/// register, base is not mutated, so a **single** entry guard can cover the
/// whole block (super-fast path).
#[derive(Debug, Clone, Copy)]
pub(super) struct BlockStackPinPlan {
    /// GPR index of the stack base (4 = RSP, 5 = RBP).
    pub base_idx: usize,
    /// Minimum signed displacement of any access in the block.
    pub min_disp: i64,
    /// Exclusive end offset: `max(disp_i + size_i)` over all accesses.
    pub max_end: i64,
    /// Any access needs data read rights on the pin.
    pub needs_r: bool,
    /// Any access needs data write rights on the pin.
    pub needs_w: bool,
}

/// Analyse body (+ optional term insn) for block-wide stack-pin eligibility.
///
/// Returns `None` when any memop is not simple stack-relative, bases differ,
/// the base register is written, stack ops modify RSP, or there is no memory.
#[must_use]
pub(super) fn analyze_block_stack_pin(
    body: &[DecodedInsn],
    term_insn: Option<&DecodedInsn>,
) -> Option<BlockStackPinPlan> {
    let mut base_reg: Option<Register> = None;
    let mut min_disp = i64::MAX;
    let mut max_end = i64::MIN;
    let mut needs_r = false;
    let mut needs_w = false;
    let mut saw_mem = false;

    let mut consider = |instr: &Instruction| -> Option<()> {
        if insn_modifies_stack_ptr(instr) {
            return None;
        }
        // Reject if this insn writes RSP/RBP before we know which base we need —
        // checked again once base is known.
        if let Some(b) = base_reg
            && insn_writes_full_gpr(instr, b)
        {
            return None;
        }
        for op in 0..instr.op_count() {
            if instr.op_kind(op) != OpKind::Memory {
                continue;
            }
            // LEA is not a memory access (handled as non-mem in lowerable set);
            // still skip if we ever see it tagged as Memory.
            if instr.mnemonic() == Mnemonic::Lea {
                continue;
            }
            let b = instr.memory_base();
            if !is_stack_base_reg(b) || instr.memory_index() != Register::None {
                return None;
            }
            // RIP-relative is not a stack pin.
            if b == Register::RIP || b == Register::EIP {
                return None;
            }
            match base_reg {
                None => base_reg = Some(b),
                Some(prev) if prev != b => return None,
                Some(_) => {}
            }
            if insn_writes_full_gpr(instr, b) {
                return None;
            }
            let disp = mem_disp_i64(instr);
            let size = u64::from(mem_width_bytes(instr).ok()?);
            let size_i = i64::try_from(size).ok()?;
            let end = disp.checked_add(size_i)?;
            min_disp = min_disp.min(disp);
            max_end = max_end.max(end);
            let (r, w) = mem_op_rw(instr, op);
            needs_r |= r;
            needs_w |= w;
            saw_mem = true;
        }
        Some(())
    };

    for d in body {
        consider(&d.instr)?;
    }
    if let Some(t) = term_insn {
        // Call/ret touch the stack — not eligible.
        if matches!(
            t.instr.mnemonic(),
            Mnemonic::Call | Mnemonic::Ret | Mnemonic::Retf
        ) {
            return None;
        }
        consider(&t.instr)?;
    }

    if !saw_mem || min_disp == i64::MAX || max_end == i64::MIN {
        return None;
    }
    // Empty / inverted span (should not happen).
    if max_end <= min_disp {
        return None;
    }
    let base_reg = base_reg?;
    let base_idx = stack_base_gpr_index(base_reg)?;
    Some(BlockStackPinPlan {
        base_idx,
        min_disp,
        max_end,
        needs_r,
        needs_w,
    })
}

#[inline]
fn is_stack_base_reg(r: Register) -> bool {
    // Soft-translate is VA-based: any access whose effective address falls in the
    // stack region is valid for the stack pin / super path, whether addressed via
    // RSP or RBP. (RBP is not a reserved frame pointer on Win64, but the
    // block-wide guard still requires `[base+disp…] ⊆ stack pin`.)
    matches!(
        r,
        Register::RSP | Register::ESP | Register::RBP | Register::EBP
    )
}

#[inline]
fn stack_base_gpr_index(r: Register) -> Option<usize> {
    match r {
        Register::RSP | Register::ESP => Some(4),
        Register::RBP | Register::EBP => Some(5),
        _ => None,
    }
}

/// iced stores displacements as the bit-pattern of a signed offset.
#[inline]
fn mem_disp_i64(instr: &Instruction) -> i64 {
    i64::from_ne_bytes(instr.memory_displacement64().to_ne_bytes())
}

fn insn_modifies_stack_ptr(instr: &Instruction) -> bool {
    matches!(
        instr.mnemonic(),
        Mnemonic::Push
            | Mnemonic::Pop
            | Mnemonic::Pushfq
            | Mnemonic::Popfq
            | Mnemonic::Pushf
            | Mnemonic::Popf
            | Mnemonic::Call
            | Mnemonic::Ret
            | Mnemonic::Retf
            | Mnemonic::Enter
            | Mnemonic::Leave
    )
}

/// Whether `instr` writes the full 64-bit `reg` (or a GPR that aliases it).
fn insn_writes_full_gpr(instr: &Instruction, reg: Register) -> bool {
    if insn_modifies_stack_ptr(instr)
        && matches!(
            reg,
            Register::RSP | Register::ESP | Register::RBP | Register::EBP
        )
    {
        // PUSH/POP/CALL/RET always update RSP; LEAVE updates RBP+RSP.
        if matches!(instr.mnemonic(), Mnemonic::Leave) {
            return matches!(
                reg,
                Register::RSP | Register::ESP | Register::RBP | Register::EBP
            );
        }
        return matches!(reg, Register::RSP | Register::ESP);
    }
    // Read-only memops / compares never write GPRs as op0.
    if matches!(
        instr.mnemonic(),
        Mnemonic::Cmp
            | Mnemonic::Test
            | Mnemonic::Bt
            | Mnemonic::Bts
            | Mnemonic::Btr
            | Mnemonic::Btc
    ) {
        // BTS/BTR/BTC write the mem/reg destination — handle below.
        if matches!(
            instr.mnemonic(),
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Bt
        ) {
            return false;
        }
    }
    if instr.op_count() == 0 {
        return false;
    }
    if instr.op0_kind() != OpKind::Register {
        return false;
    }
    let dst = instr.op_register(0);
    gpr_aliases(dst, reg)
}

fn gpr_aliases(a: Register, b: Register) -> bool {
    if a == b {
        return true;
    }
    // Same full register family (RAX/EAX/AX/AL, …).
    full_gpr(a) == full_gpr(b)
}

fn full_gpr(r: Register) -> Option<Register> {
    // Full 64-bit home for partial GPRs (RAX family, …).
    if matches!(
        r,
        Register::RAX | Register::EAX | Register::AX | Register::AL | Register::AH
    ) {
        return Some(Register::RAX);
    }
    if matches!(
        r,
        Register::RCX | Register::ECX | Register::CX | Register::CL | Register::CH
    ) {
        return Some(Register::RCX);
    }
    if matches!(
        r,
        Register::RDX | Register::EDX | Register::DX | Register::DL | Register::DH
    ) {
        return Some(Register::RDX);
    }
    if matches!(
        r,
        Register::RBX | Register::EBX | Register::BX | Register::BL | Register::BH
    ) {
        return Some(Register::RBX);
    }
    if matches!(
        r,
        Register::RSP | Register::ESP | Register::SP | Register::SPL
    ) {
        return Some(Register::RSP);
    }
    if matches!(
        r,
        Register::RBP | Register::EBP | Register::BP | Register::BPL
    ) {
        return Some(Register::RBP);
    }
    if matches!(
        r,
        Register::RSI | Register::ESI | Register::SI | Register::SIL
    ) {
        return Some(Register::RSI);
    }
    if matches!(
        r,
        Register::RDI | Register::EDI | Register::DI | Register::DIL
    ) {
        return Some(Register::RDI);
    }
    if matches!(
        r,
        Register::R8 | Register::R8D | Register::R8W | Register::R8L
    ) {
        return Some(Register::R8);
    }
    if matches!(
        r,
        Register::R9 | Register::R9D | Register::R9W | Register::R9L
    ) {
        return Some(Register::R9);
    }
    if matches!(
        r,
        Register::R10 | Register::R10D | Register::R10W | Register::R10L
    ) {
        return Some(Register::R10);
    }
    if matches!(
        r,
        Register::R11 | Register::R11D | Register::R11W | Register::R11L
    ) {
        return Some(Register::R11);
    }
    if matches!(
        r,
        Register::R12 | Register::R12D | Register::R12W | Register::R12L
    ) {
        return Some(Register::R12);
    }
    if matches!(
        r,
        Register::R13 | Register::R13D | Register::R13W | Register::R13L
    ) {
        return Some(Register::R13);
    }
    if matches!(
        r,
        Register::R14 | Register::R14D | Register::R14W | Register::R14L
    ) {
        return Some(Register::R14);
    }
    if matches!(
        r,
        Register::R15 | Register::R15D | Register::R15W | Register::R15L
    ) {
        return Some(Register::R15);
    }
    Option::None
}

/// Read/write intent for a memory operand at index `op`.
fn mem_op_rw(instr: &Instruction, op: u32) -> (bool, bool) {
    let m = instr.mnemonic();
    // Pure stores: MOV/MOVZX/… with dest mem — still only write for MOV.
    if op == 0 {
        match m {
            Mnemonic::Mov
            | Mnemonic::Movd
            | Mnemonic::Movq
            | Mnemonic::Movaps
            | Mnemonic::Movapd
            | Mnemonic::Movups
            | Mnemonic::Movupd
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movss
            | Mnemonic::Movsd
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Movzx
            | Mnemonic::Sete
            | Mnemonic::Setne
            | Mnemonic::Seta
            | Mnemonic::Setae
            | Mnemonic::Setb
            | Mnemonic::Setbe
            | Mnemonic::Setg
            | Mnemonic::Setge
            | Mnemonic::Setl
            | Mnemonic::Setle
            | Mnemonic::Seto
            | Mnemonic::Setno
            | Mnemonic::Sets
            | Mnemonic::Setns
            | Mnemonic::Setp
            | Mnemonic::Setnp => (false, true),
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Bt => (true, false),
            // RMW ALU / shifts / etc.
            _ => (true, true),
        }
    } else {
        // Source memory operand — load.
        (true, false)
    }
}

/// Byte width of a memory operand (1/2/4/8/16).
pub(super) fn mem_width_bytes(instr: &Instruction) -> Result<u32, LowerError> {
    // Prefer iced's size table (covers Packed128_*, Float64, UInt128, …).
    let table_sz = instr.memory_size().size();
    if matches!(table_sz, 1 | 2 | 4 | 8 | 16) {
        return Ok(u32::try_from(table_sz).unwrap_or(0));
    }
    let sz = match instr.memory_size() {
        MemorySize::UInt8 | MemorySize::Int8 => 1,
        MemorySize::UInt16 | MemorySize::Int16 => 2,
        MemorySize::UInt32 | MemorySize::Int32 => 4,
        MemorySize::UInt64 | MemorySize::Int64 | MemorySize::QwordOffset | MemorySize::SegPtr64 => {
            8
        }
        MemorySize::UInt128 | MemorySize::Int128 | MemorySize::Float128 => 16,
        _ => {
            if instr.op0_kind() == OpKind::Register {
                let r = instr.op_register(0);
                if r.is_xmm() { 16 } else { r.size() }
            } else if instr.op1_kind() == OpKind::Register {
                let r = instr.op_register(1);
                if r.is_xmm() { 16 } else { r.size() }
            } else {
                return Err(LowerError::form("memory_size"));
            }
        }
    };
    if matches!(sz, 1 | 2 | 4 | 8 | 16) {
        Ok(u32::try_from(sz).unwrap_or(0))
    } else {
        Err(LowerError::form("memory_width"))
    }
}
