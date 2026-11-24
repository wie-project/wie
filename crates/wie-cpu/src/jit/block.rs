//! Decode a lowerable straight-line block for Cranelift (GPR + simple mem + jcc/jmp term).

use super::lower::err::{self, LowerError};
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
    /// Near indirect `call r/m64` — push `return_ip`, exit at a target the
    /// lowering computes at run time from the register or memory operand.
    ///
    /// Separate from [`BlockTerm::Call`] because there is no compile-time
    /// target: the exit emits no chain edge (same shape as a `ret`, whose exit
    /// RIP is also a runtime value). The return address is still constant.
    CallDyn { return_ip: u64 },
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
    // The instruction that ended the walk because the operation-class table
    // does not admit it. Kept so the `NotPure` verdict below can name its
    // reason: without this the gate refuses a block and the report has nothing
    // to say about why (see `err::note_gate_refusal`).
    let mut refused: Option<Instruction> = None;

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
                // Dynamic target; placeholder for decode end only (not used
                // as fallthrough).
                BlockTerm::CallDyn { .. } | BlockTerm::Ret => next,
            };
            break;
        }

        if !is_lowerable(&instr) {
            refused = Some(instr);
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
        // `NotPure` because of the instruction we stopped at, not because of a
        // host-stop window or an unreadable page (`refused` stays `None` for
        // those): this is the case that runs at interpreter speed forever, so
        // it is the one the refusal report has to be able to name.
        if let Some(instr) = &refused {
            err::note_gate_refusal(instr);
        }
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
        TermForm::Call => {
            let return_ip = instr.next_ip();
            if matches!(
                instr.op0_kind(),
                OpKind::NearBranch16 | OpKind::NearBranch32 | OpKind::NearBranch64
            ) {
                Some(BlockTerm::Call {
                    target: instr.near_branch_target(),
                    return_ip,
                })
            } else {
                // `call r/m64`: the gate admitted it, so the operand is a
                // register or memory shape whose value the lowering reads.
                Some(BlockTerm::CallDyn { return_ip })
            }
        }
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
/// the base register is written **anywhere** in the block, stack ops modify
/// RSP, or there is no memory.
///
/// The whole-block base-write rule is the one the lowering cannot do without.
/// [`super::lower::emit_block_wide_stack_guard`] proves a range on the base
/// register's value at **block entry**, and
/// [`super::lower::mem::call_load`]'s super path then computes
/// `host = (pin.host_base - pin.guest_base) + addr` with no per-access probe.
/// Both are only about the same addresses if `addr = entry_base + disp` for
/// every access — so a base register that the block assigns to is not a stack
/// base at all, whatever iced calls it:
///
/// ```text
/// 48 89 d8        mov  rax, rbx
/// 48 6b c0 28     imul rax, rax, 0x28
/// 48 bd <imm64>   mov  rbp, <guest pointer>    ; rbp is NOT a stack base here
/// 48 01 c5        add  rbp, rax
/// 4c 39 55 00     cmp  [rbp], r10              ; reads through that pointer
/// ```
///
/// That is the planted in-guest `GetFileSize` helper
/// (`wie-runtime/src/guest_io.rs`, `getfs_impl_va + 0x17`), and it is why
/// `WIE_JIT_SUPER=all` used to die with SIGSEGV on `7za h`: the entry guard
/// passed on the *caller's* `rbp` (a real stack address) while the emitted
/// load used the pointer the block had just built, so the biased host address
/// landed in the guest address space. A check that only fires once a base has
/// already been seen misses every write that comes before the first access,
/// which is exactly this shape.
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
    // Whole-block writes to the two candidate bases, in `[RSP, RBP]` order.
    // Order-independent on purpose: a write either side of the accesses
    // invalidates the same entry-value proof.
    let mut base_written = [false; 2];

    let mut consider = |instr: &Instruction| -> Option<()> {
        if insn_modifies_stack_ptr(instr) {
            return None;
        }
        // Reject if this insn writes RSP/RBP before we know which base we need —
        // checked again once base is known, and again over the whole block
        // below.
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

    let mut note_base_writes = |instr: &Instruction| {
        let rsp = insn_writes_gpr(instr, Register::RSP);
        let rbp = insn_writes_gpr(instr, Register::RBP);
        base_written[0] |= rsp;
        base_written[1] |= rbp;
    };

    for d in body {
        consider(&d.instr)?;
        note_base_writes(&d.instr);
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
        note_base_writes(&t.instr);
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
    // The entry guard proves a range on the base register's entry value; a
    // block that writes that register anywhere has accesses relative to a
    // different value, so the proof is about the wrong address.
    let written = match base_idx {
        4 => base_written[0],
        5 => base_written[1],
        _ => true,
    };
    if written {
        return None;
    }
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

/// Whether `instr` may write `reg` (or a GPR that aliases it) — **any** operand
/// position, not just the destination operand 0.
///
/// [`insn_writes_full_gpr`] answers the narrower question "is operand 0 this
/// register", which is what the per-access checks want. The block-wide
/// stack-pin analysis needs the wider one: it is proving that a register keeps
/// its entry value for the whole block, and a missed write there puts a guest
/// pointer into the super path's bare host load (see
/// [`analyze_block_stack_pin`]). Errs toward "writes": a false positive costs a
/// block its super path, a false negative costs a SIGSEGV.
fn insn_writes_gpr(instr: &Instruction, reg: Register) -> bool {
    if insn_writes_full_gpr(instr, reg) {
        return true;
    }
    // Read-modify-write forms whose non-zero operand is also a destination:
    // `xchg r/m, r` and `xadd r/m, r` write both sides, so with a memory
    // destination operand 0 the register is still written.
    if matches!(instr.mnemonic(), Mnemonic::Xchg | Mnemonic::Xadd) {
        return (0..instr.op_count()).any(|op| {
            instr.op_kind(op) == OpKind::Register
                && instr.op_register(op) != Register::None
                && gpr_aliases(instr.op_register(op), reg)
        });
    }
    false
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

#[cfg(test)]
mod tests {
    use super::{DecodedInsn, analyze_block_stack_pin};
    use iced_x86::{Decoder, DecoderOptions};

    /// Decode `bytes` into the block-body form the analysis takes.
    fn decode(bytes: &[u8]) -> Vec<DecodedInsn> {
        let mut dec = Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE);
        let mut out = Vec::new();
        while dec.can_decode() {
            out.push(DecodedInsn {
                instr: dec.decode(),
            });
        }
        out
    }

    /// The body of the planted in-guest `GetFileSize` helper
    /// (`wie-runtime/src/guest_io.rs`, `getfs_impl_va + 0x17`):
    ///
    /// ```text
    /// mov  rax, rbx
    /// imul rax, rax, 0x28        ; slot stride
    /// mov  rbp, 0x7000_0040_3000 ; handle-table guest VA — rbp is not a stack base
    /// add  rbp, rax
    /// cmp  [rbp], r10            ; reads through that pointer
    /// jne  ..
    /// ```
    const GETFILESIZE_LOOP_BODY: [u8; 26] = [
        0x48, 0x89, 0xd8, // mov rax, rbx
        0x48, 0x6b, 0xc0, 0x28, // imul rax, rax, 0x28
        0x48, 0xbd, 0x00, 0x30, 0x40, 0x00, 0x00, 0x70, 0x00,
        0x00, // mov rbp, 0x7000_0040_3000
        0x48, 0x01, 0xc5, // add rbp, rax
        0x4c, 0x39, 0x55, 0x00, // cmp [rbp+0], r10
        0x75, 0x00, // jne +0
    ];

    /// The exact shape that made `WIE_JIT_SUPER=all` segfault on `7za h`.
    ///
    /// The block writes RBP *before* its only memory access, so the entry-value
    /// guard passed on the caller's RBP while the emitted bare host load used
    /// the table pointer the block had just built — a far-out-of-pin guest
    /// address plus the stack pin's bias. It must not be eligible.
    #[test]
    fn block_that_builds_its_own_base_is_not_stack_pinned() {
        let body = decode(&GETFILESIZE_LOOP_BODY);
        assert_eq!(body.len(), 6);
        let (term, instructions) = body.split_last().expect("non-empty fixture");
        assert!(
            analyze_block_stack_pin(instructions, Some(term)).is_none(),
            "a block that assigns RBP and then dereferences it has no stack base"
        );
        // Same instructions with the access removed: still no plan (nothing to
        // prove), which pins that the rejection is not coming from the access.
        assert!(analyze_block_stack_pin(&instructions[..3], None).is_none());
    }

    /// Positive control: an unmodified RBP used as the base *is* eligible, so
    /// the rule above cannot be passing by rejecting everything.
    #[test]
    fn plain_stack_access_still_plans() {
        // mov rax, [rbp-8] ; add rax, rcx ; jne ..
        let body = decode(&[0x48, 0x8b, 0x45, 0xf8, 0x48, 0x01, 0xc8, 0x75, 0x00]);
        let (term, instructions) = body.split_last().expect("non-empty fixture");
        let plan = analyze_block_stack_pin(instructions, Some(term)).expect("stack-only block");
        assert_eq!(plan.base_idx, 5, "RBP");
        assert_eq!(plan.min_disp, -8);
        assert_eq!(plan.max_end, 0);
        assert!(
            plan.needs_r && !plan.needs_w,
            "a load needs read rights only"
        );
    }

    /// A base written *between* two accesses is the same unsoundness as one
    /// written before them: the guard proves the entry value, the accesses use
    /// two different values.
    #[test]
    fn base_written_between_accesses_is_not_stack_pinned() {
        // mov rax, [rbp] ; add rbp, 8 ; mov rcx, [rbp] ; jne ..
        let body = decode(&[
            0x48, 0x8b, 0x45, 0x00, 0x48, 0x83, 0xc5, 0x08, 0x48, 0x8b, 0x4d, 0x00, 0x75, 0x00,
        ]);
        let (term, instructions) = body.split_last().expect("non-empty fixture");
        assert!(analyze_block_stack_pin(instructions, Some(term)).is_none());
    }

    /// `xchg r/m64, r64` writes both sides, so a memory destination hides a
    /// register write from the operand-0 check alone.
    #[test]
    fn xchg_through_memory_writes_the_base() {
        // xchg [rbp+0], rbp
        let body = decode(&[0x48, 0x87, 0x6d, 0x00, 0x75, 0x00]);
        let (term, instructions) = body.split_last().expect("non-empty fixture");
        assert!(analyze_block_stack_pin(instructions, Some(term)).is_none());
    }
}
