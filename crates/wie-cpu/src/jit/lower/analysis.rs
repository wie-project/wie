//! Live-variable analysis and XMM/GPR load tracking for one decoded block.

use super::super::config::JitConfig;
use super::OFF_XMM;
use super::emit::MemEnv;
use super::flags::iconst_u64;
use super::gpr::reg_index;

use super::super::block::{BlockTerm, DecodedInsn};
use super::err::LowerError;
use super::opclass;

use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use iced_x86::{Instruction, Mnemonic, OpKind, Register};

pub(super) fn analyze_live_gprs(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut live = [false; 16];
    for d in insns {
        mark_insn_gprs(&d.instr, &mut live);
    }
    live
}

pub(super) fn analyze_live_xmm(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut live = [false; 16];
    for d in insns {
        mark_insn_xmm(&d.instr, &mut live);
    }
    live
}

pub(super) fn analyze_def_xmm(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut defs = [false; 16];
    for d in insns {
        // Destination is typically op0 for SSE ops that write XMM.
        if d.instr.op_count() > 0
            && d.instr.op_kind(0) == OpKind::Register
            && d.instr.op_register(0).is_xmm()
        {
            let n = d.instr.op_register(0).number();
            if n < 16 {
                defs[n] = true;
            }
        }
    }
    defs
}

pub(super) fn mark_insn_xmm(instr: &Instruction, live: &mut [bool; 16]) {
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Register {
            let r = instr.op_register(i);
            if r.is_xmm() {
                let n = r.number();
                if n < 16 {
                    live[n] = true;
                }
            }
        }
    }
}

pub(super) fn load_xmm_pair(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    idx: usize,
    xmm: &mut [Value; 32],
    loaded: &mut [bool; 16],
) {
    if loaded[idx] {
        return;
    }
    let base = i64::from(OFF_XMM) + i64::try_from(idx.saturating_mul(16)).unwrap_or(0);
    let p = bcx.ins().iadd_imm(ctx_ptr, base);
    if JitConfig::get().simd_enabled() {
        // Single 128-bit load → Neon Q reg; split for lo/hi SSA compatibility.
        let v = bcx.ins().load(types::I8X16, flags, p, 0);
        let as_i64x2 = bcx
            .ins()
            .bitcast(types::I64X2, super::bitcast_flags(flags), v);
        xmm[idx * 2] = bcx.ins().extractlane(as_i64x2, 0);
        xmm[idx * 2 + 1] = bcx.ins().extractlane(as_i64x2, 1);
    } else {
        let plo = p;
        let phi = bcx.ins().iadd_imm(ctx_ptr, base + 8);
        xmm[idx * 2] = bcx.ins().load(types::I64, flags, plo, 0);
        xmm[idx * 2 + 1] = bcx.ins().load(types::I64, flags, phi, 0);
    }
    loaded[idx] = true;
}

// `mark_xmm_dirty_ir` used to emit a `load / or / store` on `JitCtx.xmm_dirty_bits`
// per XMM def. Removed: `CompiledBlock::xmm_may_def_mask` is a static superset of
// what those RMWs computed, and the host exit path now always ORs `xmm_dirty_bits`
// with `xmm_may_def_mask`, so trampolines still contribute their dynamic dirty bits.
// Skipping the per-def RMW eliminates a JitCtx aliasing edge that was blocking
// Cranelift LICM/CSE on sticky/pin metadata inside SSE-heavy loop bodies.

pub(super) fn pair_to_i8x16(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    lo: Value,
    hi: Value,
) -> Value {
    let zero = iconst_u64(bcx, 0);
    let mut v = bcx.ins().splat(types::I64X2, zero);
    v = bcx.ins().insertlane(v, lo, 0);
    v = bcx.ins().insertlane(v, hi, 1);
    bcx.ins()
        .bitcast(types::I8X16, super::bitcast_flags(flags), v)
}

pub(super) fn i8x16_to_pair(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    v: Value,
) -> (Value, Value) {
    let as_i64x2 = bcx
        .ins()
        .bitcast(types::I64X2, super::bitcast_flags(flags), v);
    let lo = bcx.ins().extractlane(as_i64x2, 0);
    let hi = bcx.ins().extractlane(as_i64x2, 1);
    (lo, hi)
}

pub(super) fn ensure_xmm_loaded(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    instr: &Instruction,
    xmm: &mut [Value; 32],
    loaded: &mut [bool; 16],
) {
    let mut need = [false; 16];
    mark_insn_xmm(instr, &mut need);
    for i in 0..16 {
        if need[i] && !loaded[i] {
            load_xmm_pair(bcx, ctx_ptr, flags, i, xmm, loaded);
        }
    }
}

/// A packed op's result in both forms: the vector it actually computed, when the
/// op ran on the vector unit, plus the lo/hi halves the SSA bank stores.
///
/// `vec` is `Some` exactly when the op was lowered to a vector instruction. The
/// store path then writes that vector as-is instead of rebuilding one from the
/// halves — the halves were *extracted from it* a moment earlier, so the rebuild
/// was pure work. When the op ran in a host helper there is no vector to reuse
/// and `vec` is `None`.
pub(super) struct PackedXmm {
    pub vec: Option<Value>,
    pub lo: Value,
    pub hi: Value,
}

/// Write XMM lo/hi SSA and immediately store into JitCtx (fault-safe write-through).
pub(super) fn store_xmm_pair(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    xmm: &mut [Value; 32],
    idx: usize,
    lo: Value,
    hi: Value,
) {
    store_xmm_packed(bcx, mem, xmm, idx, PackedXmm { vec: None, lo, hi });
}

/// [`store_xmm_pair`] with the vector form supplied when the op still has it.
///
/// One guest packed op used to cost pack(a) + pack(b) + op + unpack + **repack**:
/// `store_xmm_pair` rebuilt a vector from the two halves that `vec_to_pair` had
/// just extracted from the result vector. Handing that vector straight to the
/// store removes a splat, two `insertlane`s and a `bitcast` per op, and writes
/// the same 16 bytes — the write-through still happens immediately, which is the
/// fault-semantics guard this path exists for.
pub(super) fn store_xmm_packed(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    xmm: &mut [Value; 32],
    idx: usize,
    packed: PackedXmm,
) {
    let PackedXmm { vec, lo, hi } = packed;
    xmm[idx * 2] = lo;
    xmm[idx * 2 + 1] = hi;
    let base = i64::from(OFF_XMM) + i64::try_from(idx.saturating_mul(16)).unwrap_or(0);
    let p = bcx.ins().iadd_imm(mem.ctx_ptr, base);
    if JitConfig::get().simd_enabled() {
        let v = vec.unwrap_or_else(|| pair_to_i8x16(bcx, mem.flags, lo, hi));
        bcx.ins().store(mem.flags, v, p, 0);
    } else {
        let phi = bcx.ins().iadd_imm(mem.ctx_ptr, base + 8);
        bcx.ins().store(mem.flags, lo, p, 0);
        bcx.ins().store(mem.flags, hi, phi, 0);
    }
    // No `xmm_dirty_bits` RMW: `xmm_may_def_mask` (computed statically at compile)
    // covers this def, and the host exit path ORs both masks unconditionally.
}

pub(super) fn xmm_index(reg: Register) -> Result<usize, LowerError> {
    if !reg.is_xmm() {
        return Err(LowerError::form("xmm_register"));
    }
    let n = reg.number();
    if n < 16 {
        Ok(n)
    } else {
        Err(LowerError::form("xmm_index"))
    }
}

pub(super) fn read_xmm_pair(
    xmm: &[Value; 32],
    reg: Register,
) -> Result<(Value, Value), LowerError> {
    let i = xmm_index(reg)?;
    Ok((xmm[i * 2], xmm[i * 2 + 1]))
}

pub(super) fn block_needs_flags(insns: &[DecodedInsn], term: Option<BlockTerm>) -> bool {
    if matches!(term, Some(BlockTerm::Jcc { .. })) {
        return true;
    }
    // One row per mnemonic in [`opclass`] answers this, including the REP
    // string ops (SCAS/CMPS write flags and all of them read DF). It used to be
    // a 66-name list here, plus a second half in
    // `lower::compile_block`'s implicit-operand pass for `CmpXchg`/`Xadd`;
    // keeping the two halves in agreement by hand is what ADR-0007 removed.
    insns.iter().any(|d| opclass::needs_flags(&d.instr))
}

/// Whether the block can read the incoming FLAGS **before** it writes them.
///
/// This is the question a self-loop's carried flag word actually depends on: the
/// loop header parameter exists so a consumer at the top of the body can read
/// the flags the previous iteration left. If the body's first flag-touching
/// instruction is a writer (the normal shape — a `sub`/`add` near the top, its
/// consumer at the bottom), the incoming word is dead and passing it costs a
/// full pack per iteration for nothing.
///
/// Conservative on the two edges that matter: a block with **no** flag-touching
/// instruction answers `true` (its terminator, a `jcc`, reads the incoming
/// flags), and a `ReadWrite` use answers `true`.
pub(super) fn flags_live_in(insns: &[DecodedInsn]) -> bool {
    for d in insns {
        // `iced` carries the read/write split per instruction; `rflags_modified`
        // alone answers "writer", so no second name list can drift from it.
        let reads = d.instr.rflags_read() != 0;
        let writes = d.instr.rflags_modified() != 0;
        if reads || writes {
            return reads;
        }
    }
    // No flag-touching instruction in the body: whatever the terminator reads is
    // the incoming word.
    true
}

/// Whether this block needs the incoming RFLAGS **decoded** into per-flag i1s
/// at entry, or can carry the word itself.
///
/// The entry decode is six `band` + six `icmp` + six `iconst` — twenty host
/// instructions — and a loop head pays it on every iteration. In a loop whose
/// only readers of the individual bits are the **cold** fault packs, that whole
/// cone exists to be re-encoded: the fault pack masks the same word with
/// `~CF|~PF|~AF|~ZF|~SF|~OF` and ORs the six bits back in, so decode-then-pack is
/// an identity round trip that never changes a bit. Measured on `0x1800cef0b`
/// (a ten-instruction dword-fill loop): 20 of 177 host instructions per
/// iteration, 11.3 %, from a slot the loop never writes.
///
/// So the decode is gated. It is kept whenever anything *hot* can observe an
/// individual bit **before this block writes all six of them**, and skipped
/// otherwise; skipping leaves [`FlagState`] holding the carried word with its
/// `packed` memo set, which makes an exit or fault pack the identity (see
/// `FlagState::carried`).
///
/// # Why this is a scan and not a dataflow answer
///
/// The decoded instruction list is in hand before lowering begins, and the
/// question — "is there a hot reader before the first writer that defines all
/// six bits?" — has a conservative answer that needs no fixpoint: keep the
/// decode unless *every* flag-touching instruction in the block is one whose
/// lowering assigns all six fields from its own operands. Anything the scan does
/// not recognise, and any writer that could carry an entry bit forward, keeps the
/// decode.
///
/// # The shapes that keep it, and why each one has to
///
/// * `iced`'s `rflags_read()` is non-zero — `adc`/`sbb` (CF), `jcc`, `setcc`,
///   `cmovcc`, `pushfq`, `lahf`, `rcl`/`rcr`, and the string ops.
/// * A `jcc` terminator, which is not in the body list at all.
/// * A writer that does **not** define all six *in this lowering*. iced is not a
///   sufficient oracle here: it reports `and`/`or`/`xor`/`test` as modifying all
///   six (AF is architecturally undefined, so iced says "modified"), while
///   `assign_logic` deliberately leaves `af` untouched, so AF would reach a later
///   pack from the entry state. Same for `shl` (our `assign_shift` keeps the old
///   CF/OF/ZS/PF when `count % bits == 0`, and derives OF from the old value
///   otherwise), for `imul`/`mul` (ZF/SF/AF/PF are kept), and for `inc`/`dec`
///   (CF is kept — iced's modified mask 0x2f vs `add`'s 0x3f confirms).
///
/// The failure direction is deliberate: an unrecognised mnemonic keeps the
/// decode. A missed optimisation costs twenty instructions per block visit; a
/// wrongly skipped decode hands a fault, or a successor, a word with cleared flag
/// bits.
pub(super) fn entry_decode_needed(insns: &[DecodedInsn], term: Option<BlockTerm>) -> bool {
    if matches!(term, Some(BlockTerm::Jcc { .. })) {
        return true;
    }
    for d in insns {
        if d.instr.rflags_read() != 0 {
            return true;
        }
        if d.instr.rflags_modified() != 0 && !defines_all_six(d.instr.mnemonic()) {
            return true;
        }
    }
    false
}

/// Mnemonics whose lowering assigns **all six** architectural flag fields from
/// the instruction's own operands, leaving nothing for a later pack to inherit
/// from the entry state.
///
/// `assign_arith` covers `Add`/`Sub`/`Cmp`/`Neg` (neg is `0 - a` as a `Sub`) and
/// `assign_adc`/`assign_sbb` cover the carry forms — but `adc`/`sbb` also *read*
/// CF, so [`entry_decode_needed`] rejects them one clause earlier and they only
/// appear here so the "which writer defines everything" question has one answer.
fn defines_all_six(m: Mnemonic) -> bool {
    matches!(
        m,
        Mnemonic::Add
            | Mnemonic::Sub
            | Mnemonic::Cmp
            | Mnemonic::Neg
            | Mnemonic::Adc
            | Mnemonic::Sbb
    )
}

pub(super) fn block_has_mem(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| {
        let m = d.instr.mnemonic();
        if matches!(
            m,
            Mnemonic::Push
                | Mnemonic::Pop
                | Mnemonic::Pushfq
                | Mnemonic::Popfq
                | Mnemonic::Leave
                | Mnemonic::Call
                | Mnemonic::Ret
        ) {
            return true;
        }
        if opclass::is_string_op(&d.instr) {
            return true;
        }
        for i in 0..d.instr.op_count() {
            if d.instr.op_kind(i) == OpKind::Memory {
                return true;
            }
        }
        false
    })
}

pub(super) fn block_has_string(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| opclass::is_string_op(&d.instr))
}

pub(super) fn block_has_fp(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| {
        matches!(
            d.instr.mnemonic(),
            Mnemonic::Addss
                | Mnemonic::Subss
                | Mnemonic::Mulss
                | Mnemonic::Divss
                | Mnemonic::Addsd
                | Mnemonic::Subsd
                | Mnemonic::Mulsd
                | Mnemonic::Divsd
                | Mnemonic::Addps
                | Mnemonic::Subps
                | Mnemonic::Mulps
                | Mnemonic::Divps
                | Mnemonic::Addpd
                | Mnemonic::Subpd
                | Mnemonic::Mulpd
                | Mnemonic::Divpd
        )
    })
}

pub(super) fn mark_insn_gprs(instr: &Instruction, live: &mut [bool; 16]) {
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Register
            && let Ok(idx) = reg_index(instr.op_register(i))
        {
            live[idx] = true;
        }
    }
    // Operands the decode does not name: RSP for push/pop/call/ret, RAX/RDX
    // for the sign-extend family and `div`/`idiv`, RAX for `cmpxchg`, and
    // RAX/RCX/RSI/RDI for the REP string ops. The table is the single source for
    // those, and it is the same row the admission gate and the flags analysis
    // read — so admitting a mnemonic can no longer leave its implicit operands
    // behind, which is the bug shape `CmpXchg` and `Div`/`Idiv` each shipped.
    for &idx in opclass::implicit_gprs(instr) {
        live[idx] = true;
    }
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Memory || instr.mnemonic() == Mnemonic::Lea {
            mark_mem_regs(instr, live);
            break;
        }
    }
}

pub(super) fn mark_mem_regs(instr: &Instruction, live: &mut [bool; 16]) {
    let base = instr.memory_base();
    if base != Register::None
        && base != Register::RIP
        && base != Register::EIP
        && let Ok(idx) = reg_index(base)
    {
        live[idx] = true;
    }
    let index = instr.memory_index();
    if index != Register::None
        && let Ok(idx) = reg_index(index)
    {
        live[idx] = true;
    }
}

pub(super) fn ensure_gprs_loaded(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    gpr: &mut [Value; 16],
    loaded: &mut [bool; 16],
    instr: &Instruction,
    flags: MemFlagsData,
) {
    let mut need = [false; 16];
    mark_insn_gprs(instr, &mut need);
    for i in 0..16 {
        if need[i] && !loaded[i] {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            gpr[i] = bcx.ins().load(types::I64, flags, p, 0);
            loaded[i] = true;
        }
    }
}

#[cfg(test)]
mod flag_gate_tests {
    use super::*;
    use iced_x86::{Decoder, DecoderOptions};

    fn decode(bytes: &[u8]) -> Instruction {
        let mut dec = Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE);
        let instr = dec.decode();
        assert!(!instr.is_invalid(), "{bytes:02x?} must decode");
        instr
    }

    /// [`block_needs_flags`] is what gates `pass_flags` in `compile_block`, and
    /// `pass_flags` is what gates the SSA [`super::flags::FlagState`] — so this
    /// test is the *reason* no second "has a flag consumer" gate is needed, and
    /// it is written down rather than assumed.
    ///
    /// The opclass `flags` column answers true for readers and writers alike, so
    /// a block whose only flag use is a consumer (a `cmov`/`setcc` in the body,
    /// an `adc`/`sbb` reading the carry-in) already carries flags, and the entry
    /// `decompose_packed` is paid once for every consumer in the block rather
    /// than once per consumer. The terminator is a separate question: a `jcc`
    /// never appears in `insns` (it ends the block), which is why the `term`
    /// argument exists.
    #[test]
    fn every_flag_consumer_makes_the_block_carry_flags() {
        const CONSUMERS: [(&str, &[u8]); 8] = [
            ("cmovne ecx, edx", &[0x0F, 0x45, 0xCA]),
            ("sete cl", &[0x0F, 0x94, 0xC1]),
            ("adc edx, ecx", &[0x11, 0xCA]),
            ("sbb edx, ecx", &[0x19, 0xCA]),
            ("pushfq", &[0x9C]),
            ("cmpxchg [rbx], eax", &[0x0F, 0xB1, 0x03]),
            ("xadd [rbx], eax", &[0x0F, 0xC1, 0x03]),
            ("comiss xmm0, xmm1", &[0x0F, 0x2F, 0xC1]),
        ];
        for (name, bytes) in CONSUMERS {
            let insns = [DecodedInsn {
                instr: decode(bytes),
            }];
            assert!(
                block_needs_flags(&insns, None),
                "{name} reads flags, so the block must carry them"
            );
        }

        // A `jcc` terminator is not part of `insns` — it ends the block.
        let at = 0x1002;
        assert!(block_needs_flags(
            &[],
            Some(BlockTerm::Jcc {
                mnemonic: Mnemonic::Je,
                taken: at,
                not_taken: at,
            })
        ));

        // The negative control: a block that touches no flag and has no
        // flag-reading successor must NOT pay for the carrier.
        let insns = [DecodedInsn {
            instr: decode(&[0x89, 0xD8]),
        }];
        assert!(
            !block_needs_flags(&insns, None),
            "mov is not a flag instruction"
        );
    }
}

// The entry-decode gate is a guard on a fault-visible value, so its table is
// here rather than in the differential suite: the question "does this block need
// the incoming word decoded" is answered from the decoded list alone, and each
// row below is one shape whose answer has to stay on the conservative side.
#[cfg(test)]
mod entry_decode_tests {
    use super::entry_decode_needed;
    use crate::jit::block::{BlockTerm, DecodedInsn};
    use iced_x86::{Decoder, DecoderOptions};

    /// Decode `bytes` into the block shape the predicate sees.
    fn block(bytes: &[u8]) -> Vec<DecodedInsn> {
        let mut dec = Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE);
        let mut out = Vec::new();
        while dec.can_decode() {
            let instr = dec.decode();
            if instr.is_invalid() {
                break;
            }
            out.push(DecodedInsn { instr });
        }
        out
    }

    fn jcc() -> Option<BlockTerm> {
        Some(BlockTerm::Jcc {
            mnemonic: iced_x86::Mnemonic::Jne,
            taken: 0x1000,
            not_taken: 0x1004,
        })
    }

    /// `(name, bytes, terminator, decode needed)`.
    ///
    /// The `false` rows are the whole point: they are the shapes where nothing hot
    /// can read a bit before a writer replaces all six, so the carried word makes
    /// the packs identities. Every other row keeps the decode, including the ones
    /// `iced` calls "modified" on all six but this lowering does not
    /// (`and`/`or`/`xor`/`test` keep AF, `imul` keeps ZF/SF/AF/PF, `inc`/`dec`
    /// keep CF, shifts keep several).
    const ROWS: &[(&str, &[u8], bool, bool)] = &[
        // --- skipped: nothing hot reads a bit, every writer defines all six ---
        ("no flag insn at all", &[0x89, 0xD8], false, false), // mov eax,ebx
        ("add only", &[0x01, 0xD8], false, false),
        (
            "add, sub, neg, cmp",
            &[0x01, 0xD8, 0x29, 0xD8, 0xF7, 0xD8, 0x39, 0xD8],
            false,
            false,
        ),
        ("add then store", &[0x01, 0xD8, 0x89, 0x07], false, false),
        // --- kept: a hot reader ---
        ("add + jcc terminator", &[0x01, 0xD8], true, true),
        ("mov + jcc terminator", &[0x89, 0xD8], true, true),
        ("adc reads CF", &[0x11, 0xD8], false, true),
        ("sbb reads CF", &[0x19, 0xD8], false, true),
        ("pushfq reads the word", &[0x9C], false, true),
        ("lahf reads the word", &[0x9F], false, true),
        ("setb reads CF", &[0x0F, 0x92, 0xC1], false, true),
        ("cmovb reads CF", &[0x0F, 0x42, 0xC1], false, true),
        ("rcl reads CF", &[0xD1, 0xD0], false, true),
        // --- kept: a writer that can carry an entry bit forward ---
        ("inc preserves CF", &[0xFF, 0xC0], false, true),
        ("dec preserves CF", &[0xFF, 0xC8], false, true),
        ("and keeps AF in this lowering", &[0x21, 0xD8], false, true),
        ("or keeps AF in this lowering", &[0x09, 0xD8], false, true),
        ("xor keeps AF in this lowering", &[0x31, 0xD8], false, true),
        ("test keeps AF in this lowering", &[0x85, 0xD8], false, true),
        ("shl reads the old CF/OF/ZS", &[0xD1, 0xE0], false, true),
        ("imul keeps ZF/SF/AF/PF", &[0x0F, 0xAF, 0xC3], false, true),
        ("stosd (string op)", &[0xAB], false, true),
        // --- kept: not recognised as a writer of all six, and the safe
        // direction is to decode ---
        (
            "comiss writes the byte without being an assign_arith writer",
            &[0x0F, 0x2F, 0xC1],
            false,
            true,
        ),
        ("bt writes CF only", &[0x0F, 0xA3, 0xD8], false, true),
        // --- skipped: touches no flag at all ---
        ("rdtsc touches no flag", &[0x0F, 0x31], false, false),
    ];

    #[test]
    fn the_gate_keeps_the_decode_except_for_the_pure_writer_shapes() {
        for (name, bytes, term, expected) in ROWS {
            let insns = block(bytes);
            assert!(!insns.is_empty(), "{name}: fixture did not decode");
            let term = if *term { jcc() } else { None };
            assert_eq!(
                entry_decode_needed(&insns, term),
                *expected,
                "{name}: entry_decode_needed(term={term:?}) should be {expected}"
            );
        }
    }
}
