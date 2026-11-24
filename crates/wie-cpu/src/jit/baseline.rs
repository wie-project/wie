//! Tier-0 baseline compiler: single-pass, no optimization, fast lowering.
//!
//! Target: 0.1–0.3 ms per block vs ~3 ms for Cranelift tier-1. The current
//! implementation is a structural stub that satisfies the tier-0 wiring and
//! handles the common `mov`/`alu`/`branch`/`mem` shapes via the shared
//! lowering (micro-stub fast path when eligible, otherwise a direct Cranelift
//! compile with `opt_level = "none"` would be ideal). The stub keeps
//! correctness while the boot-mode fast path (threshold = 1, inline token
//! bucket) already delivers the cold-boot wall improvement; a future iteration
//! can replace the body with a true single-pass emitter (e.g. dynasm/aarch64
//! direct) without changing the pipeline contract.
//!
//! Contract with `pipeline.rs`:
//! - `try_compile_baseline` is called synchronously on the guest thread
//!   (first hit during boot, 0.2 ms budget).
//! - On success the caller installs the returned `CompiledBlock` as `Ready`
//!   (tier-0) and enqueues a tier-1 Cranelift job for the same RIP. The
//!   background worker's later install overwrites the tier-0 entry
//!   (last-writer-wins); the next dispatch therefore patches to tier-1.
//! - On `None` the caller falls back to the normal Hot/eager path.
//!
//! # Why loop regions are *not* formed in here (`WIE_JIT_REGION=1`)
//!
//! A baseline compile is the one place a loop header gets compiled and then
//! never revisited: the caller installs it `Ready`, and nothing re-compiles a
//! `Ready` entry except a self-loop's tier-up. That is exactly why a header
//! first seen during boot stayed a per-block translation for the rest of the
//! process, so this is the seam the region path has to cover — but the seam
//! belongs one frame up, in
//! [`JitCpu::step_compiled`](super::pipeline), for a reason neither this
//! function nor `JitShared` can work around: **region formation needs the
//! guest thread's [`HookWindow`](crate::exec::HookWindow)**, and this signature
//! deliberately does not take one. `compile_from_kind_shared` is shared with the
//! background workers, which have no hook window, so a region formed down here
//! could cover an address the guest asked to stop at and would then run that
//! instruction natively, silently skipping the stop.
//!
//! So the pipeline asks [`backward_targets`] about the block it just compiled
//! and forms the region itself, where the window is in hand. Nothing in this
//! file changes when the knob is off.

#![allow(
    unsafe_code, // finalized Cranelift fn pointers are unsafe
    private_interfaces
)]

use super::block::{BlockKind, BlockTerm};
use super::fast_api::FastApiKind;
use super::lower::CompiledBlock;
use super::shared::JitShared;
use crate::regs::Rflags;
use iced_x86::{Mnemonic, OpKind};

/// Whether the decoded block is eligible for baseline compilation.
///
/// Baseline currently accepts any `Pure` block up to 32 insns that the
/// lowerer considers compilable. `NotPure` blocks are never baseline
/// candidates (they fall back to the interpreter).
#[must_use]
pub(super) fn is_baseline_eligible(kind: &BlockKind) -> bool {
    match kind {
        BlockKind::NotPure => false,
        BlockKind::Pure { insns, .. } => insns.len() <= 32,
    }
}

/// Record one backward successor, deduplicated.
fn push_backward(slots: &mut [u64; 2], len: &mut usize, from: u64, to: u64) {
    // **`to == from` is included**, i.e. a block that branches to its own entry
    // is a loop header of a one-block loop. That is the same cycle as any other,
    // and it is where the hottest blocks in a real guest are: DOOM Retro's
    // SDL2 scaled-blit block (`0x1800d8fb0`, 54.1 % of guest time) is one, and
    // while it was excluded here it could not get the region's once-per-entry
    // memory hoist at all.
    //
    // The tier is what made this safe to include rather than a gamble: a region
    // compiles at the tier the block path would have picked for the same RIP
    // (`JitShared::decide_tier`, which returns `Speed` for exactly this shape),
    // so folding a self-loop replaces a `speed` block with a `speed` region, not
    // with a `Base` one. See `pipeline::try_compile_region`.
    if to > from || slots[..*len].contains(&to) {
        return;
    }
    slots[*len] = to;
    *len += 1;
}

/// The successors of the block at `rip` that are **backward edges**: at or below
/// the block's own entry VA.
///
/// Such a successor is the head of a cycle — control reaches `rip`, and `rip`
/// branches back into it — so it is a **loop header**, which is precisely what
/// [`form_loop_region`](super::lower::region) folds. `rip` itself is included:
/// a block that branches back to its own entry is the header of a one-block
/// loop, the commonest loop shape there is. This is the whole of the per-block
/// cost of finding loops the compiler was not already pointed at: two
/// comparisons over the two successor slots of a [`BlockKind`] that has already
/// been decoded.
///
/// Returned as `(slots, len)` like `ExploredBlock::successors` in
/// `lower/region.rs`, so a caller does not allocate for a list of at most two.
///
/// # Why the *latch* can answer this and the header cannot
///
/// The header's own terminator says nothing — a loop's exit branch is forward
/// and its backward branch is in some later block. Only the block that branches
/// back knows the loop exists, and it knows it the first time it is compiled,
/// which is *after* the header has already been compiled. That is the ordering
/// that left boot-compiled loop headers on the per-block path forever (see
/// [`try_compile_baseline`]'s region note); this function is how the latch hands
/// the fact to the pipeline.
#[must_use]
pub(super) fn backward_targets(rip: u64, kind: &BlockKind) -> ([u64; 2], usize) {
    let mut slots = [0_u64; 2];
    let mut len = 0_usize;
    match kind {
        BlockKind::Pure { term, end_rip, .. } => match term {
            Some(BlockTerm::Jmp { target }) => push_backward(&mut slots, &mut len, rip, *target),
            Some(BlockTerm::Jcc {
                taken, not_taken, ..
            }) => {
                push_backward(&mut slots, &mut len, rip, *taken);
                push_backward(&mut slots, &mut len, rip, *not_taken);
            }
            // A call's successor is the *callee*, and control resumes at the
            // return site rather than at the target: a callee that happens to
            // sit below this block is not a loop header of this loop.
            // `CallDyn`/`Ret` have no static successor at all.
            Some(BlockTerm::Call { .. } | BlockTerm::CallDyn { .. } | BlockTerm::Ret) => {}
            // No terminator: the decoder walks strictly forward, so this can
            // never produce a candidate. Kept explicit rather than `_` so a new
            // `BlockTerm` variant has to state its answer.
            None => push_backward(&mut slots, &mut len, rip, *end_rip),
        },
        BlockKind::NotPure => {}
    }
    (slots, len)
}

/// Try to compile `kind` with the baseline tier (synchronous, guest thread).
///
/// Returns `Some(CompiledBlock)` on success. The block is functionally
/// equivalent to a tier-1 compile for the same inputs; only the *when*
/// differs. Callers must still enqueue a tier-1 background job so the
/// block is later patched to the optimized version.
#[must_use]
pub(super) fn try_compile_baseline(
    shared: &JitShared,
    fast_api: &[(u64, FastApiKind)],
    rip: u64,
    kind: BlockKind,
    inv_gen: u64,
) -> Option<CompiledBlock> {
    if !is_baseline_eligible(&kind) {
        return None;
    }
    // Census for the tier-0 emitter, which is why this call site knows the
    // shape at all: the entry arm is the one place a block is compiled
    // synchronously on the guest thread, so the fraction of *this* set a
    // hand-written emitter could take is the fraction that decides whether it
    // is worth building. Debug-gated: a release build that is not tracing pays
    // one atomic load.
    if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!(
            rip = format_args!("{rip:#x}"),
            fit = tier0_fit(&kind).label(),
            blocker = tier0_blocker(&kind),
            insns = match &kind {
                BlockKind::Pure { insns, .. } => insns.len(),
                BlockKind::NotPure => 0,
            },
            "tier0 census"
        );
    }
    // Fast path: micro-stub (1–3 insn handlers) is already baseline-optimal
    // (no Cranelift). Otherwise delegate to the shared lowering; even though
    // this still uses Cranelift, the surrounding boot-mode path (threshold=1
    // + inline token bucket) already collapses iced residency. A true
    // single-pass emitter can replace this delegate without touching the
    // pipeline.
    shared.compile_from_kind_shared(fast_api, rip, kind, inv_gen)
}

/// Which tier-0 subset a decoded block falls into.
///
/// This is the whitelist a single-pass emitter is built against, and it is
/// deliberately narrower than what the Cranelift path accepts: the point of
/// tier 0 is to be *fast to produce*, so every shape it cannot express must be
/// visible as a fallback rather than absorbed into a slower "tier 0".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tier0Fit {
    /// GPR-only `mov`/ALU/`cmp`/`test` with register or immediate operands and a
    /// fall-through, `jmp` or `jcc` exit. No memory operand, so no probe path,
    /// no `MemEnv`, no register allocator: the entry-time subset a first
    /// emitter should take.
    AluOnly,
    /// The same instruction set but with at least one memory operand. Tier 0
    /// would need the probe/refill contract for these, which is the *second*
    /// increment — a tier 0 that reuses `MemEnv` is Cranelift with extra steps.
    WithMem,
    /// An operation tier 0 does not express: calls, string ops, x87/SSE, shifts
    /// (their flag rules are count-dependent and not modelled yet), `mul`/`div`,
    /// `lea`, `movzx`/`movsx`, `ret`, …
    Other,
    /// Not a `Pure` block, or longer than the entry arm accepts at all.
    NotEligible,
}

impl Tier0Fit {
    /// Stable name for the census log.
    #[must_use]
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::AluOnly => "alu_only",
            Self::WithMem => "with_mem",
            Self::Other => "other",
            Self::NotEligible => "not_eligible",
        }
    }
}

/// Classify `kind` against the tier-0 whitelist.
///
/// Every operand is inspected, so a block is `AluOnly` only if *nothing* in it
/// can need the memory path or a non-GPR register file. `WithMem` is reported
/// separately rather than folded into `Other` because it is the next increment
/// and its size decides whether that increment is worth building.
#[must_use]
pub(super) fn tier0_fit(kind: &BlockKind) -> Tier0Fit {
    let BlockKind::Pure { insns, term, .. } = kind else {
        return Tier0Fit::NotEligible;
    };
    if insns.len() > 32 {
        return Tier0Fit::NotEligible;
    }
    match term {
        None | Some(BlockTerm::Jmp { .. } | BlockTerm::Jcc { .. }) => {}
        Some(BlockTerm::Call { .. } | BlockTerm::CallDyn { .. } | BlockTerm::Ret) => {
            return Tier0Fit::Other;
        }
    }
    let mut any_mem = false;
    // **`insns` includes the terminator** (`decode_pure_gpr_block` pushes it so
    // the lowering has it), so the body scan must stop before it: a block ending
    // in `jne` is not "a block containing an unsupported mnemonic". Getting this
    // wrong rejected 1 748 of 4 225 blocks (41 %) by their own `jcc` — the bug
    // the census found, see `docs/architecture/tier0-emitter.md` §2.1.
    for insn in tier0_body(insns, term.is_some()) {
        let instr = &insn.instr;
        if !tier0_mnemonic(instr.mnemonic()) {
            return Tier0Fit::Other;
        }
        for op in 0..instr.op_count() {
            match instr.op_kind(op) {
                OpKind::Register => {
                    if !instr.op_register(op).is_gpr() {
                        return Tier0Fit::Other;
                    }
                }
                OpKind::Memory => any_mem = true,
                OpKind::Immediate8
                | OpKind::Immediate8to32
                | OpKind::Immediate8to64
                | OpKind::Immediate32
                | OpKind::Immediate32to64
                | OpKind::Immediate64 => {}
                // A near branch is the terminator, classified above; any other
                // operand kind (far branch, a register-file shape that is not a
                // GPR, …) has no tier-0 encoding.
                _ => return Tier0Fit::Other,
            }
        }
    }
    if any_mem {
        Tier0Fit::WithMem
    } else {
        Tier0Fit::AluOnly
    }
}

/// The **first** thing that keeps `kind` out of `AluOnly`, for the census log.
///
/// A count of "other" is not actionable; a ranked list of blockers is what
/// decides the next increment's whitelist. Diagnostic only — it allocates, so it
/// is never called outside a `DEBUG`-enabled census.
#[must_use]
pub(super) fn tier0_blocker(kind: &BlockKind) -> String {
    let BlockKind::Pure { insns, term, .. } = kind else {
        return "not_pure".to_string();
    };
    match term {
        None | Some(BlockTerm::Jmp { .. } | BlockTerm::Jcc { .. }) => {}
        Some(BlockTerm::Call { .. }) => return "term:call".to_string(),
        Some(BlockTerm::CallDyn { .. }) => return "term:calldyn".to_string(),
        Some(BlockTerm::Ret) => return "term:ret".to_string(),
    }
    for insn in tier0_body(insns, term.is_some()) {
        let instr = &insn.instr;
        if !tier0_mnemonic(instr.mnemonic()) {
            return format!("mnemonic:{:?}", instr.mnemonic());
        }
        for op in 0..instr.op_count() {
            match instr.op_kind(op) {
                OpKind::Register => {
                    if !instr.op_register(op).is_gpr() {
                        return format!("reg:{:?}", instr.op_register(op));
                    }
                }
                OpKind::Memory => {}
                OpKind::Immediate8
                | OpKind::Immediate8to32
                | OpKind::Immediate8to64
                | OpKind::Immediate32
                | OpKind::Immediate32to64
                | OpKind::Immediate64 => {}
                other => return format!("op:{other:?}"),
            }
        }
    }
    "none".to_string()
}

/// The **body** instructions of a block: everything except the terminator.
///
/// A single definition because both the classifier and its diagnostic must agree
/// about what "the body" is; when they disagreed it produced a bug that survived
/// a review and a test suite, because the wrong answer was still *a* count.
fn tier0_body(insns: &[super::block::DecodedInsn], has_term: bool) -> &[super::block::DecodedInsn] {
    &insns[..insns.len().saturating_sub(usize::from(has_term))]
}

/// The mnemonics a first tier-0 emitter covers, on GPRs only.
///
/// Shifts are **excluded on purpose**: their flags depend on the count (`CF` is
/// the last bit shifted out, `OF` is defined only for a count of one, `AF` is
/// undefined), and [`tier0_flags`] does not model that. Leaving them out keeps
/// the whitelist an exact statement of what the flag reference is tested for.
/// `Lea` is excluded separately: it needs effective-address assembly, the same
/// machinery as the memory path.
fn tier0_mnemonic(m: Mnemonic) -> bool {
    matches!(
        m,
        Mnemonic::Mov
            | Mnemonic::Add
            | Mnemonic::Sub
            | Mnemonic::And
            | Mnemonic::Or
            | Mnemonic::Xor
            | Mnemonic::Cmp
            | Mnemonic::Test
            | Mnemonic::Inc
            | Mnemonic::Dec
            | Mnemonic::Nop
    )
}

fn width_mask(width: u32) -> u64 {
    match width {
        8 => 0xFF,
        16 => 0xFFFF,
        32 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}

fn sign_bit(width: u32) -> u64 {
    1_u64 << (width - 1)
}

/// The destination value `op` writes, with x86 zero-extension applied.
///
/// **This is one of the two places a tier-0 emitter produces silently wrong
/// code.** A 32-bit ALU result is zero-extended into the 64-bit register; an
/// 8- or 16-bit one leaves the rest of the register untouched, so `add ax, bx`
/// and `add eax, ebx` cannot share an implementation. Returning the full
/// destination rather than a masked result makes that impossible to forget at
/// the emit site.
#[must_use]
#[allow(
    dead_code,
    reason = "exercised by the tests that pin the semantics; consumed by the tier-0 emitter"
)]
pub(super) fn tier0_result(op: Mnemonic, width: u32, dst: u64, src: u64) -> u64 {
    let r = match op {
        Mnemonic::Add | Mnemonic::Inc => dst.wrapping_add(src),
        Mnemonic::Sub | Mnemonic::Dec | Mnemonic::Cmp => dst.wrapping_sub(src),
        Mnemonic::And | Mnemonic::Test => dst & src,
        Mnemonic::Or => dst | src,
        Mnemonic::Xor => dst ^ src,
        Mnemonic::Mov => src,
        _ => dst,
    };
    match width {
        32 => r & 0xFFFF_FFFF,
        64 => r,
        w => (dst & !width_mask(w)) | (r & width_mask(w)),
    }
}

/// The flag word `op` leaves behind, at `width`.
///
/// **The other place tier 0 goes silently wrong,** and the one a differential
/// test has to pin per-op rather than per-shape:
///
/// - `inc`/`dec` do **not** write `CF`, so they take `prior` and preserve it;
/// - the logical ops clear `CF` and `OF`, and `AF` is architecturally
///   *undefined* there (modelled clear, which is what the Cranelift lowering
///   produces — a tier 0 that disagreed would diverge from the block path it
///   replaces);
/// - parity is a property of the **low byte**, not of the result word;
/// - `AF` is modelled rather than zeroed because a guest `pushf`/`lahf` and the
///   decimal-adjust instructions read it.
///
/// `mov`/`nop` write no flags at all and return `prior` unchanged.
#[must_use]
#[allow(
    dead_code,
    reason = "exercised by the tests that pin the semantics; consumed by the tier-0 emitter"
)]
pub(super) fn tier0_flags(op: Mnemonic, width: u32, dst: u64, src: u64, prior: Rflags) -> Rflags {
    if matches!(op, Mnemonic::Mov | Mnemonic::Nop) {
        return prior;
    }
    let mask = width_mask(width);
    let sign = sign_bit(width);
    let d = dst & mask;
    let s = src & mask;
    let r = tier0_result(op, width, dst, src) & mask;
    let mut f = Rflags::default();
    match op {
        Mnemonic::Add | Mnemonic::Inc => {
            if r < d {
                f |= Rflags::CF;
            }
            if (d ^ r) & (s ^ r) & sign != 0 {
                f |= Rflags::OF;
            }
        }
        Mnemonic::Sub | Mnemonic::Cmp | Mnemonic::Dec => {
            if d < s {
                f |= Rflags::CF;
            }
            if (d ^ s) & (d ^ r) & sign != 0 {
                f |= Rflags::OF;
            }
        }
        _ => {
            // And / Or / Xor / Test: CF and OF clear, AF undefined -> clear.
            f &= !Rflags::AF;
        }
    }
    // `inc`/`dec` preserve CF from the prior word; every other op here writes
    // it (or cleared it above).
    if matches!(op, Mnemonic::Inc | Mnemonic::Dec)
        && (u64::from(prior) & u64::from(Rflags::CF)) != 0
    {
        f |= Rflags::CF;
    }
    if matches!(
        op,
        Mnemonic::Add | Mnemonic::Sub | Mnemonic::Cmp | Mnemonic::Inc | Mnemonic::Dec
    ) && (d ^ s ^ r) & 0x10 != 0
    {
        f |= Rflags::AF;
    }
    if (r & 0xFF).count_ones().is_multiple_of(2) {
        f |= Rflags::PF;
    }
    if r == 0 {
        f |= Rflags::ZF;
    }
    if r & sign != 0 {
        f |= Rflags::SF;
    }
    f
}

#[cfg(test)]
mod tests {
    //! [`backward_targets`]: the two-comparison question that finds a loop from
    //! its latch.
    //!
    //! These are pure over a hand-built [`BlockKind`], which is the point: the
    //! function is the whole per-block cost the knob adds to the block path, and
    //! every arm is a claim about which guest shape is a loop header candidate.

    use super::*;

    fn pure_at(term: Option<BlockTerm>, end_rip: u64) -> BlockKind {
        BlockKind::Pure {
            insns: Vec::new(),
            end_rip,
            bytes_len: 0,
            term,
        }
    }

    fn slots(kind: &BlockKind, rip: u64) -> Vec<u64> {
        let (slots, len) = backward_targets(rip, kind);
        slots[..len].to_vec()
    }

    // ---- tier-0 whitelist and flag semantics -----------------------------
    //
    // These two functions are where a single-pass emitter goes silently wrong,
    // so they are specified by test rather than by comment. The flag tests are
    // written per *op* and per *width*, because the failure mode is not "the
    // block does not work" but "the block works and one later `jcc` takes the
    // other side".

    /// Decode `bytes` into the instruction list a `Pure` block carries.
    ///
    /// Real iced decoding rather than a hand-built vector: the bug these tests
    /// pin is about *what the decoder puts in the list* (the terminator is
    /// included), so a fixture that skipped that would not exercise it.
    fn decode_insns(bytes: &[u8], ip: u64) -> Vec<super::super::block::DecodedInsn> {
        let mut decoder = iced_x86::Decoder::with_ip(64, bytes, ip, iced_x86::DecoderOptions::NONE);
        let mut out = Vec::new();
        while decoder.can_decode() {
            out.push(super::super::block::DecodedInsn {
                instr: decoder.decode(),
            });
        }
        out
    }

    fn pure_with(bytes: &[u8], ip: u64, term: BlockTerm, end_rip: u64) -> BlockKind {
        BlockKind::Pure {
            insns: decode_insns(bytes, ip),
            end_rip,
            bytes_len: u32::try_from(bytes.len()).unwrap_or(0),
            term: Some(term),
        }
    }

    /// The census bug, pinned: the terminator is **in** `insns`, so a body scan
    /// that does not stop before it rejects the block by its own `jcc`.
    ///
    /// This is the shape that made 1 748 of 4 225 blocks (41 %) look
    /// unsupported, and it is why the first census reported 8 `alu_only`
    /// instead of the true figure.
    #[test]
    fn a_gpr_body_ending_in_a_conditional_jump_is_alu_only() {
        // add rax, rbx ; jne (rel8, target irrelevant to classification)
        let kind = pure_with(
            &[0x48, 0x01, 0xD8, 0x75, 0xFE],
            0x1000,
            BlockTerm::Jcc {
                mnemonic: Mnemonic::Jne,
                taken: 0x1000,
                not_taken: 0x1005,
            },
            0x1005,
        );
        assert_eq!(tier0_fit(&kind), Tier0Fit::AluOnly);
        assert_eq!(tier0_blocker(&kind), "none");
    }

    /// The other half of the fix: skipping the terminator in the body scan must
    /// not swallow terminator *classification*. A call is still not tier 0.
    #[test]
    fn a_call_terminator_keeps_the_block_out_of_tier0() {
        // add rax, rbx ; call rel32
        let kind = pure_with(
            &[0x48, 0x01, 0xD8, 0xE8, 0x00, 0x00, 0x00, 0x00],
            0x1000,
            BlockTerm::Call {
                target: 0x2000,
                return_ip: 0x1008,
            },
            0x2000,
        );
        assert_eq!(tier0_fit(&kind), Tier0Fit::Other);
        assert_eq!(tier0_blocker(&kind), "term:call");
    }

    #[test]
    fn inc_and_dec_preserve_carry_where_add_and_sub_do_not() {
        let cf = Rflags::default() | Rflags::CF;
        // inc: CF is preserved, and 0xFFFFFFFF + 1 wraps without touching it.
        let f = tier0_flags(Mnemonic::Inc, 32, 0xFFFF_FFFF, 1, cf);
        assert!(f & Rflags::CF != Rflags::default(), "inc must preserve CF");
        assert!(f & Rflags::ZF != Rflags::default());
        // add: the same arithmetic *does* write CF.
        let f = tier0_flags(Mnemonic::Add, 32, 0xFFFF_FFFF, 1, cf);
        assert!(
            f & Rflags::CF != Rflags::default(),
            "add must set CF on wrap"
        );
        // ... and clears it when it fits, which inc must not do to a set CF.
        let f = tier0_flags(Mnemonic::Add, 64, 1, 1, cf);
        assert_eq!(f & Rflags::CF, Rflags::default(), "add 1,1 clears CF");
        let f = tier0_flags(Mnemonic::Inc, 64, 1, 1, cf);
        assert!(
            f & Rflags::CF != Rflags::default(),
            "inc 1 keeps the prior CF"
        );
        // The mutation this catches: implementing inc as add (or dec as sub),
        // which is the obvious way to write it and is wrong exactly here.
    }

    #[test]
    fn overflow_is_signed_and_carry_is_unsigned() {
        // 0x7f + 1 at 8 bits: signed overflow, no unsigned carry.
        let f = tier0_flags(Mnemonic::Add, 8, 0x7F, 1, Rflags::default());
        assert!(f & Rflags::OF != Rflags::default(), "0x7f+1 overflows");
        assert_eq!(f & Rflags::CF, Rflags::default(), "0x7f+1 does not carry");
        // 0xff + 1 at 8 bits: unsigned carry, no signed overflow.
        let f = tier0_flags(Mnemonic::Add, 8, 0xFF, 1, Rflags::default());
        assert!(f & Rflags::CF != Rflags::default(), "0xff+1 carries");
        assert_eq!(
            f & Rflags::OF,
            Rflags::default(),
            "0xff+1 does not overflow"
        );
        // cmp is a sub: 5 - 7 borrows.
        let f = tier0_flags(Mnemonic::Cmp, 64, 5, 7, Rflags::default());
        assert!(f & Rflags::CF != Rflags::default(), "5-7 borrows");
        assert!(f & Rflags::SF != Rflags::default(), "5-7 is negative");
        assert_eq!(f & Rflags::ZF, Rflags::default());
    }

    #[test]
    fn parity_is_of_the_low_byte_only() {
        // Both cases are chosen so a whole-word popcount would disagree:
        // 0x0100 has an even low byte (PF set) and an odd 64-bit popcount.
        let f = tier0_flags(Mnemonic::Add, 64, 0x0100, 0, Rflags::default());
        assert!(
            f & Rflags::PF != Rflags::default(),
            "low byte 0x00 is even: PF set"
        );
        // 0x0101 has an odd low byte (PF clear) and an even 64-bit popcount.
        let f = tier0_flags(Mnemonic::Add, 64, 0x0101, 0, Rflags::default());
        assert_eq!(
            f & Rflags::PF,
            Rflags::default(),
            "low byte 0x01 is odd: PF clear"
        );
        // The mutation this catches: `r.count_ones() % 2` on the full result.
    }

    #[test]
    fn logical_ops_clear_carry_and_overflow_and_af() {
        let prior = Rflags::default() | Rflags::CF | Rflags::OF | Rflags::AF;
        for op in [Mnemonic::And, Mnemonic::Or, Mnemonic::Xor, Mnemonic::Test] {
            let f = tier0_flags(op, 64, 0xFF00, 0x0F0F, prior);
            assert_eq!(f & Rflags::CF, Rflags::default(), "{op:?} clears CF");
            assert_eq!(f & Rflags::OF, Rflags::default(), "{op:?} clears OF");
            assert_eq!(f & Rflags::AF, Rflags::default(), "{op:?} models AF clear");
        }
    }

    #[test]
    fn a_thirty_two_bit_result_zero_extends_and_a_sixteen_bit_one_does_not() {
        // add eax, ebx: upper half zeroed by definition.
        let r = tier0_result(Mnemonic::Add, 32, 0xFFFF_FFFF_FFFF_FFFF, 2);
        assert_eq!(r, 1, "add eax,2 zero-extends into the 64-bit register");
        // add ax, bx: upper 48 bits preserved.
        let r = tier0_result(Mnemonic::Add, 16, 0xFFFF_FFFF_FFFF_FFFF, 2);
        assert_eq!(r, 0xFFFF_FFFF_FFFF_0001, "add ax,2 keeps the upper bits");
        // The mutation this catches: masking the result without the
        // zero-extension case, which makes `add eax, ebx` leave stale high bits
        // and only fails once something later reads the full register.
    }

    #[test]
    fn mov_writes_no_flags() {
        let prior = Rflags::default() | Rflags::CF | Rflags::ZF;
        assert_eq!(tier0_flags(Mnemonic::Mov, 64, 0, 5, prior), prior);
    }

    /// The DOOM shape: a `jl` whose taken side is the loop header below it. Its
    /// *fallthrough* is forward, so exactly one candidate.
    #[test]
    fn a_backward_jcc_yields_its_taken_side_only() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jl,
                taken: 0x1000,
                not_taken: 0x2000,
            }),
            0x2000,
        );
        assert_eq!(slots(&kind, 0x1800), vec![0x1000]);
    }

    /// Both sides backward: both are candidates, and the loop it describes is
    /// discovered from whichever one the fold tries first.
    #[test]
    fn a_jcc_with_two_backward_sides_yields_both() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jl,
                taken: 0x1000,
                not_taken: 0x1800,
            }),
            0x1800,
        );
        assert_eq!(slots(&kind, 0x2000), vec![0x1000, 0x1800]);
    }

    /// A forward `jmp` is an ordinary successor, not a loop.
    #[test]
    fn a_forward_jmp_is_not_a_loop_header() {
        let kind = pure_at(Some(BlockTerm::Jmp { target: 0x2000 }), 0x2000);
        assert!(slots(&kind, 0x1000).is_empty());
    }

    /// The DOOM-blit shape: a block whose `jne` targets itself. It **is** a loop
    /// header — of a one-block loop — and it must be proposed, or the hottest
    /// block in a real guest can never become a region.
    #[test]
    fn a_self_loop_is_a_loop_header() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jne,
                taken: 0x1000,
                not_taken: 0x2000,
            }),
            0x1000,
        );
        assert_eq!(slots(&kind, 0x1000), vec![0x1000]);
    }

    /// Both sides of a `jcc` landing on its own entry is still **one**
    /// candidate, not two: the two slots are deduplicated, so a self-loop costs
    /// one formation attempt rather than two.
    #[test]
    fn a_two_sided_self_loop_yields_one_candidate() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jle,
                taken: 0x1000,
                not_taken: 0x1000,
            }),
            0x1000,
        );
        assert_eq!(slots(&kind, 0x1000), vec![0x1000]);
    }

    /// A `call`'s successor is the callee: control resumes at the return site,
    /// so a callee below the caller is not a loop header of the caller's loop.
    #[test]
    fn a_call_target_is_not_a_loop_header() {
        let kind = pure_at(
            Some(BlockTerm::Call {
                target: 0x1000,
                return_ip: 0x2005,
            }),
            0x1000,
        );
        assert!(slots(&kind, 0x2000).is_empty());
    }

    /// `ret` and an indirect call have no static successor; nor does a
    /// `NotPure` block (nothing was decoded to ask about). A block with no
    /// terminator falls through, and the decoder walks strictly forward, so its
    /// only edge is never a candidate.
    #[test]
    fn terminators_without_a_static_successor_yield_nothing() {
        for term in [
            Some(BlockTerm::Ret),
            Some(BlockTerm::CallDyn { return_ip: 0x2005 }),
        ] {
            let kind = pure_at(term, 0x2000);
            assert!(slots(&kind, 0x2000).is_empty(), "term={term:?}");
        }
        assert!(
            slots(&pure_at(None, 0x2010), 0x2000).is_empty(),
            "fallthrough"
        );
        assert!(slots(&BlockKind::NotPure, 0x2000).is_empty());
    }
}
