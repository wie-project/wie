//! One operation-class table: what the JIT admits, in which operand form, and
//! what compiling it implies about GPR liveness and RFLAGS.
//!
//! ## Why one table (ADR-[0007])
//!
//! The integer core used to answer the same per-mnemonic questions in six
//! separate matches: `block::is_lowerable` (may this compile?), `block::is_string_op`
//! and `block::string_op_size` (is this a REP string op, and how wide?),
//! `block::classify_terminator`'s `jcc` list, `analysis::block_needs_flags`
//! (does the block need an RFLAGS carrier?) and `analysis::mark_insn_gprs` +
//! the implicit-operand completion in `lower::compile_block` (which GPRs does
//! this touch without naming them?). Every one of those is a statement about
//! the *same* mnemonic, and they had to be kept in agreement by hand. A fix
//! that reached three of the four silently emitted `iconst 0` for the fourth —
//! the hazard `crate::jit::block` and `crate::jit::tests::implicit_operands`
//! both document.
//!
//! [`row`] is now the only place a mnemonic is named, and the form predicates
//! below are the only place an operand shape is decided. `Mnemonic -> OpRow`
//! carries all four answers, so adding an instruction is one table row and the
//! gate cannot disagree with the liveness analysis.
//!
//! [0007]: ../../../docs/adr/0007-opclass-table.md
//!
//! ## What this table deliberately does *not* do
//!
//! **It does not dispatch to the emitter.** Several form predicates are one
//! line, and the emitter for every admitted mnemonic is `lower_insn`'s match
//! in [`super::insn`]. Carrying the emitter here was tried on paper and
//! rejected: the emitters take twelve distinct argument lists and most carry a
//! per-mnemonic parameter (`Arith::Add`, `ShiftKind::Rol`, `FloatBinOp` +
//! width, `SseCvtOp`, …), so a table-carried emitter needs either one wrapper
//! function per mnemonic (230 wrappers — no DRY gain) or a coarse `Emitter` tag
//! plus a second mnemonic re-dispatch inside each arm, which moves the same
//! knowledge rather than removing it. ADR-0007 asks for the table to back
//! `lower_insn`'s *pre-check*, not its dispatch, and that is what it does: the
//! refusal arm at the bottom of `lower_insn` asks this table which of the two
//! failure modes it just hit, and `opclass_tests` pins
//! `admitted == emitted ⊎ string ⊎ term`, so a table row without an emitter
//! fails a test instead of silently becoming a permanent `mark_never`.
//!
//! **It does not cover `exec`.** The interpreter is not on the path where gate
//! and lowering must agree by construction, and it is already deliberately
//! divergent in 89 mnemonics (ADR-0007, "Decision").

use super::gpr::is_imm_kind;
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};

/// Implicit GPR slots, as indices into the 16-entry GPR file.
///
/// An instruction that reads or writes one of these without naming it in its
/// operands must have it marked live, or the block compiles against an
/// `iconst 0` placeholder and produces garbage with no fault.
const RAX: &[usize] = &[0];
/// `RDX:RAX` — the `div`/`idiv` dividend, and `cwd`'s sign-extension target.
const RDX_RAX: &[usize] = &[0, 2];
/// The stack pointer, which every push/pop/call/ret updates.
const RSP: &[usize] = &[4];
/// `leave`: `RSP <- RBP` then pop RBP.
const RSP_RBP: &[usize] = &[4, 5];
/// REP string ops address through `RSI`/`RDI` and count in `RCX`; `SCAS`/`CMPS`
/// also read and write RAX, `LODS`/`STOS` write it.
const STRING_GPRS: &[usize] = &[0, 1, 6, 7];

/// The operation class of a decoded instruction.
///
/// This is the ADR-0007 shape: the two engines' divergence is *between* the
/// classes, so the class is what a reader (and the tests) reason about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpClass {
    /// No architectural state at all: dropping the instruction is exact.
    Noop,
    /// Integer core: GPR / memory / immediate operand.
    Gpr(GprForm),
    /// SSE / FP: XMM register and `m32`/`m64`/`m128` operands.
    Sse(SseForm),
    /// REP string op. The decoder ends the block after one, because a
    /// `rep`-prefixed op needs a dynamic exit RIP from the host helper.
    String(StrForm),
    /// Control-flow terminator. Only ever a block's **last** instruction, and
    /// admitted by [`term_of`], never as a body instruction.
    Term(TermForm),
    /// Not compiled. The interpreter (`crate::exec`) runs it.
    Refused,
}

/// Operand contract for the integer core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GprForm {
    /// No operand-form check: the encoding has exactly one shape.
    Bare,
    /// `lea r64, m` — any EA form including full `base+index*scale+disp`.
    Lea,
    /// `mov`-shaped: reg/reg, reg/imm, reg/mem, mem/reg, mem/imm.
    Rm,
    /// [`GprForm::Rm`] plus the memory-atomicity rule (`add [rbx], eax` is a
    /// plain load+store, a `LOCK`ed one is not).
    RmAtomic,
    /// One operand that is a register or a simple memory location.
    Unary,
    /// [`GprForm::Unary`] plus the memory-atomicity rule.
    UnaryAtomic,
    /// Dst register, src register or memory: `movzx`/`movsx`/`movsxd`,
    /// `cmov` and the bit scans all take exactly this shape.
    RegRm,
    /// `push`: r64 / imm / simple memory, never a 16-bit override.
    Push,
    /// `pop`: r64 / simple memory, never a 16-bit override.
    Pop,
    /// `bswap`: register only, 32- or 64-bit.
    Bswap,
    /// `imul`: 2-operand (dst *= src) or 3-operand (dst = src * imm).
    Imul,
    /// `div`/`idiv`: 32-bit register or simple-memory divisor.
    Div,
    /// No shape check at all beyond the memory-atomicity rule (`xchg`).
    RmwOnly,
    /// Shift/rotate: dst reg or memory, count imm or CL.
    Shift,
    /// `xadd`/`cmpxchg`: dst reg/mem, src register.
    XaddCmpxchg,
}

/// Operand contract for SSE / FP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SseForm {
    /// `xmm <- xmm/m`, i.e. the destination is a register. Covers the packed,
    /// scalar and bitwise families: `xorps`, `addps`, `sqrtss`, `comiss`,
    /// `pshufd`, the packed-integer group and the packed converts.
    ///
    /// Note this is NOT the `movaps` contract: a move is three-way and admits
    /// the store direction `xmm -> m128` as well, which is [`SseForm::Mov`].
    XmmRm,
    /// `xmm <-> xmm`, `xmm <- m32/m64/m128` **and** `m32/m64/m128 <- xmm`:
    /// `movaps`/`movups`/`movdqa`/`movdqu`/`movapd`/`movupd`, `movss`,
    /// `movsd` (SSE form) and the half-lane `movlps`/`movlpd`/`movhps`/`movhpd`.
    ///
    /// The store direction is why this is a separate contract: merging it into
    /// `XmmRm` would silently stop compiling `movaps [rbx], xmm0`, which is
    /// most of a `memcpy`.
    Mov,
    /// `movq`: xmm↔xmm, xmm↔m64, xmm↔r64.
    Movq,
    /// `movd`: xmm↔r32/m32.
    Movd,
    /// `movhlps`/`movlhps`: register-to-register half-lane shuffle only.
    MovHl,
    /// `pmovmskb`/`vpmovmskb`: r32 ← sign bits, register form only.
    Movmsk,
    /// `movmskpd`/`movmskps`: r32/r64 ← sign bits of packed FP elements.
    MovmskGpr,
    /// `cvtsi2ss`/`cvtsi2sd`: dst xmm, src r32/r64 or m32/m64.
    CvtGprToXmm,
    /// `cvttss2si`/`cvtsd2si`: dst r32/r64, src xmm or m32/m64.
    CvtXmmToGpr,
    /// `psllw`-family and `pslldq`/`psrldq`: src imm8 or xmm/m128 count.
    Shift,
}

/// REP string op, carrying the element width the helper needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrForm {
    /// 1-byte elements: `stosb`, `movsb`, `lodsb`, `scasb`, `cmpsb`.
    B1,
    /// 2-byte elements: `stosw`, `movsw`, `scasw`, `cmpsw`.
    W2,
    /// 4-byte elements: `stosd`, `lodsd`, `scasd`, `cmpsd`.
    D4,
    /// 8-byte elements: `stosq`, `movsq`, `lodsq`, `scasq`, `cmpsq`.
    Q8,
    /// `movsd` in its string form (the SSE form is [`SseForm::Mov`]).
    D4String,
}

impl StrForm {
    /// Stable name of this operand contract, for a refusal report.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::B1 => "string_b1",
            Self::W2 => "string_w2",
            Self::D4 => "string_d4",
            Self::Q8 => "string_q8",
            Self::D4String => "string_d4_movsd",
        }
    }

    /// Element width in bytes.
    pub(crate) const fn width(self) -> u32 {
        match self {
            Self::B1 => 1,
            Self::W2 => 2,
            Self::D4 | Self::D4String => 4,
            Self::Q8 => 8,
        }
    }
}

/// Control-flow terminator shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermForm {
    /// Near `jmp rel`.
    Jmp,
    /// Near `jcc rel`.
    Jcc,
    /// Near `call rel`, and the near indirect `call r/m64`.
    ///
    /// The indirect form is admitted because the *block exit* already carries a
    /// runtime RIP (that is how `ret` leaves a block): the return address this
    /// call pushes is still the constant `next_ip`, so only the target is
    /// dynamic. See [`is_call_rm64`].
    Call,
    /// Near `ret` with no immediate (`ret imm16` needs an interpreter).
    Ret,
}

/// One row of the operation-class table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpRow {
    /// Admission class and operand contract.
    pub(crate) class: OpClass,
    /// GPR slots the instruction touches without naming them in its operands.
    pub(crate) implicit: &'static [usize],
    /// Whether compiling this instruction means the block must carry RFLAGS.
    pub(crate) flags: bool,
}

impl OpRow {
    const fn new(class: OpClass) -> Self {
        Self {
            class,
            implicit: &[],
            flags: false,
        }
    }

    const fn noop() -> Self {
        Self::new(OpClass::Noop)
    }

    const fn gpr(form: GprForm) -> Self {
        Self::new(OpClass::Gpr(form))
    }

    const fn sse(form: SseForm) -> Self {
        Self::new(OpClass::Sse(form))
    }

    /// Every string op reads `RAX`/`RCX`/`RSI`/`RDI` and needs the carrier:
    /// `SCAS`/`CMPS` write flags and all of them read DF.
    const fn string(form: StrForm) -> Self {
        Self {
            class: OpClass::String(form),
            implicit: STRING_GPRS,
            flags: true,
        }
    }

    const fn term(form: TermForm) -> Self {
        Self::new(OpClass::Term(form))
    }

    const fn refused() -> Self {
        Self::new(OpClass::Refused)
    }

    /// The block needs an RFLAGS carrier because of this instruction.
    const fn flags(self) -> Self {
        Self {
            class: self.class,
            implicit: self.implicit,
            flags: true,
        }
    }

    /// GPR slots this instruction touches implicitly.
    const fn implicit(self, idx: &'static [usize]) -> Self {
        Self {
            class: self.class,
            implicit: idx,
            flags: self.flags,
        }
    }

    /// May this instruction appear in a compiled **body**?
    ///
    /// A terminator is admitted by [`term_of`] instead: it ends the block, so
    /// it is never a body instruction and answering `true` here would let the
    /// decoder walk past a `jmp`.
    pub(crate) fn admits(self, instr: &Instruction) -> bool {
        match self.class {
            OpClass::Noop => true,
            OpClass::Gpr(form) => form.admits(instr),
            OpClass::Sse(form) => form.admits(instr),
            OpClass::String(_) => true,
            OpClass::Term(_) | OpClass::Refused => false,
        }
    }
}

/// `movsd` names two unrelated instructions which share one mnemonic.
///
/// The SSE2 scalar move (`F2 0F 10`) is `Sse(Mov)` and the inherited string
/// MOVS DWORD (`A5`) is `String(D4String)`. Every other row's class follows
/// from the mnemonic alone, so this one row is resolved from the operands.
const MOVSD_STRING: OpRow = OpRow::string(StrForm::D4String);

// The operation-class table.
//
// Read a row as: this mnemonic compiles, its operands must look like `form`,
// compiling it makes these GPR slots live, and (if `flags`) the block needs an
// RFLAGS carrier. Mnemonics that are absent are `Refused`: the interpreter
// runs them, which is deliberate for the x87 family, `In`/`Out*`, `Cpuid`,
// `Rdtsc`, `Ldmxcsr`/`Stmxcsr`, `Rcpps`/`Rsqrt*`, 64-bit `div`/`mul`, and any
// memory form whose atomicity the JIT cannot provide.
pub(super) const fn row(m: Mnemonic) -> OpRow {
    match m {
        // ---- No architectural state ------------------------------------
        //
        // PAUSE (F3 90) is a pipeline hint: op_count == 0, no used registers,
        // `rflags_modified() == 0`. See
        // `jit::tests::pause_tests::pause_is_architecturally_a_noop`.
        Mnemonic::Nop
        | Mnemonic::Pause
        | Mnemonic::Endbr64
        | Mnemonic::Endbr32
        | Mnemonic::Prefetchnta
        | Mnemonic::Prefetcht0
        | Mnemonic::Prefetcht1
        | Mnemonic::Prefetcht2
        | Mnemonic::Prefetchw
        | Mnemonic::Prefetchwt1 => OpRow::noop(),

        // ---- Fixed-shape integer ops: no operand check ------------------
        //
        // The accumulator sign-extend family, the direction flag, the rflags
        // stack and `leave`. Their implicit operands differ, which is exactly
        // what the `implicit` field is for.
        Mnemonic::Cwde | Mnemonic::Cdqe | Mnemonic::Cbw => OpRow::gpr(GprForm::Bare).implicit(RAX),
        Mnemonic::Cwd => OpRow::gpr(GprForm::Bare).implicit(RDX_RAX),
        Mnemonic::Cld | Mnemonic::Std => OpRow::gpr(GprForm::Bare).flags(),
        Mnemonic::Pushfq | Mnemonic::Popfq => OpRow::gpr(GprForm::Bare).implicit(RSP).flags(),
        // LEAVE: RSP <- RBP, then pop RBP. Neither register is an operand.
        Mnemonic::Leave => OpRow::gpr(GprForm::Bare).implicit(RSP_RBP),

        // ---- Address computation, data movement, stack ------------------
        Mnemonic::Lea => OpRow::gpr(GprForm::Lea),
        Mnemonic::Mov => OpRow::gpr(GprForm::Rm),
        // MOVZX/MOVSX/MOVSXD and CMOVcc decide their operand form identically
        // (dst register, src register or simple memory); only the flags
        // requirement differs.
        Mnemonic::Movzx | Mnemonic::Movsx | Mnemonic::Movsxd => OpRow::gpr(GprForm::RegRm),
        Mnemonic::Push => OpRow::gpr(GprForm::Push).implicit(RSP),
        Mnemonic::Pop => OpRow::gpr(GprForm::Pop).implicit(RSP),
        Mnemonic::Bswap => OpRow::gpr(GprForm::Bswap),
        Mnemonic::Xchg => OpRow::gpr(GprForm::RmwOnly),

        // ---- Flag-writing integer arithmetic ---------------------------
        //
        // ADD/SUB/logic/CMP/TEST/BT* and the unary INC/DEC/NEG all write flags.
        // NOT does not, which is why it is the one unary without `.flags()`.
        //
        // `rcl`/`rcr` are deliberately absent from this list: they are not in
        // the pre-ADR `block_needs_flags` set either, and adding them would
        // change emitted code. Recorded as a known gap in the follow-up notes
        // rather than fixed here.
        Mnemonic::Add
        | Mnemonic::Adc
        | Mnemonic::Sub
        | Mnemonic::Sbb
        | Mnemonic::Xor
        | Mnemonic::And
        | Mnemonic::Or
        | Mnemonic::Cmp
        | Mnemonic::Test
        | Mnemonic::Bt
        | Mnemonic::Bts
        | Mnemonic::Btr
        | Mnemonic::Btc => OpRow::gpr(GprForm::RmAtomic).flags(),
        // BTS/BTR/BTC are read-modify-write against memory but are NOT
        // implicitly locked; `RmAtomic` is the one place that decides, and an
        // explicit `LOCK` prefix is refused there.
        Mnemonic::Inc | Mnemonic::Dec | Mnemonic::Neg => OpRow::gpr(GprForm::UnaryAtomic).flags(),
        Mnemonic::Not => OpRow::gpr(GprForm::UnaryAtomic),
        Mnemonic::Imul => OpRow::gpr(GprForm::Imul).flags(),
        // DIV/IDIV are one-operand encodings whose dividend is the implicit
        // EDX:EAX pair and whose quotient/remainder land back in EAX/EDX.
        // Neither register appears in the decode, so without `implicit` the
        // block divides an `iconst 0` dividend: `div ecx` with EAX=100, ECX=7
        // gave EAX=14, EDX=2 on the interpreter and EAX=0, EDX=0 here, with no
        // fault and no diagnostic.
        Mnemonic::Div | Mnemonic::Idiv => OpRow::gpr(GprForm::Div).implicit(RDX_RAX),
        Mnemonic::Shl
        | Mnemonic::Sal
        | Mnemonic::Shr
        | Mnemonic::Sar
        | Mnemonic::Rol
        | Mnemonic::Ror => OpRow::gpr(GprForm::Shift).flags(),
        Mnemonic::Rcl | Mnemonic::Rcr => OpRow::gpr(GprForm::Shift),
        // XADD writes flags on every form; CMPXCHG also compares against the
        // implicit accumulator (AX/EAX/RAX — the ISA has no 8-bit form) and
        // writes ZF.
        Mnemonic::Xadd => OpRow::gpr(GprForm::XaddCmpxchg).flags(),
        Mnemonic::Cmpxchg => OpRow::gpr(GprForm::XaddCmpxchg).implicit(RAX).flags(),
        // Bit scans write ZF and leave the rest undefined.
        Mnemonic::Bsr | Mnemonic::Bsf | Mnemonic::Lzcnt => OpRow::gpr(GprForm::RegRm).flags(),

        // ---- Conditional moves and byte sets ---------------------------
        Mnemonic::Cmove
        | Mnemonic::Cmovne
        | Mnemonic::Cmova
        | Mnemonic::Cmovae
        | Mnemonic::Cmovb
        | Mnemonic::Cmovbe
        | Mnemonic::Cmovg
        | Mnemonic::Cmovge
        | Mnemonic::Cmovl
        | Mnemonic::Cmovle
        | Mnemonic::Cmovo
        | Mnemonic::Cmovno
        | Mnemonic::Cmovs
        | Mnemonic::Cmovns
        | Mnemonic::Cmovp
        | Mnemonic::Cmovnp => OpRow::gpr(GprForm::RegRm).flags(),
        Mnemonic::Sete
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
        | Mnemonic::Setnp => OpRow::gpr(GprForm::Unary).flags(),

        // ---- SSE moves -------------------------------------------------
        Mnemonic::Movaps
        | Mnemonic::Movups
        | Mnemonic::Movdqa
        | Mnemonic::Movdqu
        | Mnemonic::Movapd
        | Mnemonic::Movupd
        | Mnemonic::Movss
        | Mnemonic::Movlps
        | Mnemonic::Movlpd
        | Mnemonic::Movhps
        | Mnemonic::Movhpd => OpRow::sse(SseForm::Mov),
        // `movsd`: SSE2 scalar move. The string form is resolved in `row_of`.
        Mnemonic::Movsd => OpRow::sse(SseForm::Mov),
        Mnemonic::Movq => OpRow::sse(SseForm::Movq),
        Mnemonic::Movd => OpRow::sse(SseForm::Movd),
        Mnemonic::Movhlps | Mnemonic::Movlhps => OpRow::sse(SseForm::MovHl),
        Mnemonic::Pmovmskb | Mnemonic::Vpmovmskb => OpRow::sse(SseForm::Movmsk),
        Mnemonic::Movmskpd | Mnemonic::Movmskps => OpRow::sse(SseForm::MovmskGpr),

        // ---- SSE bitwise / packed integer / FP arithmetic / converts ----
        //
        // All of these are `dst xmm, src xmm/m`; the emitter distinguishes
        // them, the admission decision does not.
        Mnemonic::Xorps
        | Mnemonic::Xorpd
        | Mnemonic::Pxor
        | Mnemonic::Andps
        | Mnemonic::Andpd
        | Mnemonic::Pand
        | Mnemonic::Orps
        | Mnemonic::Orpd
        | Mnemonic::Por
        | Mnemonic::Andnps
        | Mnemonic::Andnpd
        | Mnemonic::Pandn
        | Mnemonic::Addss
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
        | Mnemonic::Sqrtss
        | Mnemonic::Sqrtsd
        | Mnemonic::Sqrtps
        | Mnemonic::Sqrtpd
        | Mnemonic::Minss
        | Mnemonic::Minsd
        | Mnemonic::Maxss
        | Mnemonic::Maxsd
        | Mnemonic::Minps
        | Mnemonic::Minpd
        | Mnemonic::Maxps
        | Mnemonic::Maxpd
        | Mnemonic::Paddb
        | Mnemonic::Paddw
        | Mnemonic::Paddd
        | Mnemonic::Paddq
        | Mnemonic::Psubb
        | Mnemonic::Psubw
        | Mnemonic::Psubd
        | Mnemonic::Psubq
        | Mnemonic::Paddsb
        | Mnemonic::Paddsw
        | Mnemonic::Paddusb
        | Mnemonic::Paddusw
        | Mnemonic::Psubsb
        | Mnemonic::Psubsw
        | Mnemonic::Psubusb
        | Mnemonic::Psubusw
        | Mnemonic::Pmullw
        | Mnemonic::Pmulhw
        | Mnemonic::Pmulhuw
        | Mnemonic::Pmuludq
        | Mnemonic::Pmaddwd
        | Mnemonic::Pcmpeqb
        | Mnemonic::Pcmpeqw
        | Mnemonic::Pcmpeqd
        | Mnemonic::Pcmpgtb
        | Mnemonic::Pcmpgtw
        | Mnemonic::Pcmpgtd
        | Mnemonic::Packsswb
        | Mnemonic::Packssdw
        | Mnemonic::Packuswb
        | Mnemonic::Psadbw
        | Mnemonic::Punpcklbw
        | Mnemonic::Punpcklwd
        | Mnemonic::Punpckldq
        | Mnemonic::Punpckhbw
        | Mnemonic::Punpckhwd
        | Mnemonic::Punpckhdq
        | Mnemonic::Punpcklqdq
        | Mnemonic::Punpckhqdq
        | Mnemonic::Unpcklpd
        | Mnemonic::Unpckhpd
        | Mnemonic::Pshufd
        | Mnemonic::Pshuflw
        | Mnemonic::Pshufhw
        | Mnemonic::Shufpd
        | Mnemonic::Pshufb
        | Mnemonic::Cvtps2dq
        | Mnemonic::Cvtdq2ps
        | Mnemonic::Cvttps2dq
        | Mnemonic::Cvtdq2pd
        | Mnemonic::Cvtps2pd
        | Mnemonic::Cvtpd2dq
        | Mnemonic::Cvtpd2ps
        | Mnemonic::Cvttpd2dq
        | Mnemonic::Cvtsd2ss
        | Mnemonic::Cvtss2sd => OpRow::sse(SseForm::XmmRm),
        // Packed shifts: imm8, or a variable count in xmm/m128.
        Mnemonic::Psllw
        | Mnemonic::Pslld
        | Mnemonic::Psllq
        | Mnemonic::Psrlw
        | Mnemonic::Psrld
        | Mnemonic::Psrlq
        | Mnemonic::Psraw
        | Mnemonic::Psrad
        | Mnemonic::Psrldq
        | Mnemonic::Pslldq => OpRow::sse(SseForm::Shift),
        Mnemonic::Cvtsi2ss | Mnemonic::Cvtsi2sd => OpRow::sse(SseForm::CvtGprToXmm),
        Mnemonic::Cvttss2si | Mnemonic::Cvtss2si | Mnemonic::Cvttsd2si | Mnemonic::Cvtsd2si => {
            OpRow::sse(SseForm::CvtXmmToGpr)
        }
        // FP compare → RFLAGS.
        Mnemonic::Comiss | Mnemonic::Ucomiss | Mnemonic::Comisd | Mnemonic::Ucomisd => {
            OpRow::sse(SseForm::XmmRm).flags()
        }

        // ---- REP string ops --------------------------------------------
        //
        // `lodsw` is absent, and `movsd` is the operand-resolved override:
        // both asymmetries predate this table and are preserved, not fixed.
        Mnemonic::Stosb | Mnemonic::Movsb | Mnemonic::Lodsb | Mnemonic::Scasb | Mnemonic::Cmpsb => {
            OpRow::string(StrForm::B1)
        }
        Mnemonic::Stosw | Mnemonic::Movsw | Mnemonic::Scasw | Mnemonic::Cmpsw => {
            OpRow::string(StrForm::W2)
        }
        Mnemonic::Stosd | Mnemonic::Lodsd | Mnemonic::Scasd | Mnemonic::Cmpsd => {
            OpRow::string(StrForm::D4)
        }
        Mnemonic::Stosq | Mnemonic::Movsq | Mnemonic::Lodsq | Mnemonic::Scasq | Mnemonic::Cmpsq => {
            OpRow::string(StrForm::Q8)
        }

        // ---- Control flow ----------------------------------------------
        Mnemonic::Jmp => OpRow::term(TermForm::Jmp),
        Mnemonic::Je
        | Mnemonic::Jne
        | Mnemonic::Ja
        | Mnemonic::Jae
        | Mnemonic::Jb
        | Mnemonic::Jbe
        | Mnemonic::Jg
        | Mnemonic::Jge
        | Mnemonic::Jl
        | Mnemonic::Jle
        | Mnemonic::Jo
        | Mnemonic::Jno
        | Mnemonic::Js
        | Mnemonic::Jns
        | Mnemonic::Jp
        | Mnemonic::Jnp => OpRow::term(TermForm::Jcc),
        Mnemonic::Call => OpRow::term(TermForm::Call).implicit(RSP),
        Mnemonic::Ret => OpRow::term(TermForm::Ret).implicit(RSP),

        // Everything else: the interpreter runs it.
        _ => OpRow::refused(),
    }
}

/// The table row for one decoded instruction.
///
/// Differs from [`row`] only for the single dual-form mnemonic (`movsd`).
pub(crate) fn row_of(instr: &Instruction) -> OpRow {
    if instr.mnemonic() == Mnemonic::Movsd && !sse_movsd_is_sse(instr) {
        return MOVSD_STRING;
    }
    row(instr.mnemonic())
}

/// May this instruction be compiled into a block **body**?
///
/// The single admission gate. `crate::jit::block::decode_pure_gpr_block` is the
/// only caller, and asking this question anywhere else is how the gate and the
/// lowering drifted apart before ADR-0007.
pub(crate) fn admits(instr: &Instruction) -> bool {
    row_of(instr).admits(instr)
}

/// Whether the table has any row for `m` other than `Refused`.
///
/// A `Refused` mnemonic must never reach the lowerer; anything else may. This is
/// what lets the refusal in `lower_insn` say which of the two happened.
pub(crate) fn admits_mnemonic(m: Mnemonic) -> bool {
    !matches!(row(m).class, OpClass::Refused)
}

/// The terminator shape of `instr`, if it terminates a block.
pub(crate) fn term_of(instr: &Instruction) -> Option<TermForm> {
    match row_of(instr).class {
        OpClass::Term(form) if form.admits(instr) => Some(form),
        _ => None,
    }
}

/// True for MOVS/STOS/LODS/SCAS/CMPS, including the `movsd` string form.
pub(crate) fn is_string_op(instr: &Instruction) -> bool {
    matches!(row_of(instr).class, OpClass::String(_))
}

/// Element width of a string op, or `None` when `instr` is not one.
pub(crate) fn string_op_size(instr: &Instruction) -> Option<u32> {
    match row_of(instr).class {
        OpClass::String(form) => Some(form.width()),
        _ => None,
    }
}

/// Whether compiling `instr` makes the block need an RFLAGS carrier.
pub(crate) fn needs_flags(instr: &Instruction) -> bool {
    row_of(instr).flags
}

/// GPR slots `instr` touches without naming them in its operands.
pub(crate) fn implicit_gprs(instr: &Instruction) -> &'static [usize] {
    row_of(instr).implicit
}

impl GprForm {
    /// Stable name of this operand contract, for a refusal report.
    ///
    /// Names the *admitted* shape, which is what a refusal needs to say: an
    /// instruction refused against `gpr_rm_atomic` was refused because it is
    /// not that shape, and the shape name tells a reader what the lowering
    /// expects without re-reading this file.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Bare => "gpr_bare",
            Self::Lea => "gpr_lea",
            Self::Rm => "gpr_rm",
            Self::RmAtomic => "gpr_rm_atomic",
            Self::Unary => "gpr_unary",
            Self::UnaryAtomic => "gpr_unary_atomic",
            Self::RegRm => "gpr_reg_rm",
            Self::Push => "gpr_push",
            Self::Pop => "gpr_pop",
            Self::Bswap => "gpr_bswap",
            Self::Imul => "gpr_imul",
            Self::Div => "gpr_div",
            Self::RmwOnly => "gpr_rmw",
            Self::Shift => "gpr_shift",
            Self::XaddCmpxchg => "gpr_xadd_cmpxchg",
        }
    }

    fn admits(self, instr: &Instruction) -> bool {
        match self {
            Self::Bare => true,
            Self::Lea => lea_is_simple(instr),
            Self::Rm => rm_imm_shape(instr),
            Self::RmAtomic => rm_imm_shape(instr) && mem_rmw_is_lowerable(instr),
            Self::Unary => unary_shape(instr),
            Self::UnaryAtomic => unary_shape(instr) && mem_rmw_is_lowerable(instr),
            Self::RegRm => reg_rm_shape(instr),
            Self::Push => push_is_lowerable(instr),
            Self::Pop => pop_is_lowerable(instr),
            Self::Bswap => {
                matches!(instr.op0_kind(), OpKind::Register)
                    && matches!(instr.op_register(0).size(), 4 | 8)
            }
            Self::Imul => imul_is_lowerable(instr),
            Self::Div => div_is_lowerable(instr),
            Self::RmwOnly => mem_rmw_is_lowerable(instr),
            Self::Shift => shift_is_lowerable(instr) && mem_rmw_is_lowerable(instr),
            Self::XaddCmpxchg => match (instr.op0_kind(), instr.op1_kind()) {
                (OpKind::Register, OpKind::Register) => true,
                (OpKind::Memory, OpKind::Register) => {
                    mem_ea_ok(instr) && mem_size_ok(instr) && mem_rmw_is_lowerable(instr)
                }
                _ => false,
            },
        }
    }
}

impl SseForm {
    /// Stable name of this operand contract, for a refusal report.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::XmmRm => "sse_xmm_rm",
            Self::Mov => "sse_mov",
            Self::Movq => "sse_movq",
            Self::Movd => "sse_movd",
            Self::MovHl => "sse_movhl",
            Self::Movmsk => "sse_movmsk",
            Self::MovmskGpr => "sse_movmsk_gpr",
            Self::CvtGprToXmm => "sse_cvt_gpr",
            Self::CvtXmmToGpr => "sse_cvt_xmm",
            Self::Shift => "sse_shift",
        }
    }

    fn admits(self, instr: &Instruction) -> bool {
        match self {
            Self::XmmRm => xmm_rm_is_lowerable(instr),
            Self::Mov => {
                let k0 = instr.op0_kind();
                let k1 = instr.op1_kind();
                match (k0, k1) {
                    (OpKind::Register, OpKind::Register) => {
                        instr.op_register(0).is_xmm() && instr.op_register(1).is_xmm()
                    }
                    (OpKind::Register, OpKind::Memory) => {
                        instr.op_register(0).is_xmm() && mem_ea_ok(instr) && mem_size_ok_sse(instr)
                    }
                    (OpKind::Memory, OpKind::Register) => {
                        instr.op_register(1).is_xmm() && mem_ea_ok(instr) && mem_size_ok_sse(instr)
                    }
                    _ => false,
                }
            }
            Self::Movq => sse_movq_is_lowerable(instr),
            Self::Movd => sse_movd_is_lowerable(instr),
            Self::MovHl => matches!(
                (instr.op0_kind(), instr.op1_kind()),
                (OpKind::Register, OpKind::Register)
            ),
            Self::Movmsk => matches!(
                (instr.op0_kind(), instr.op1_kind()),
                (OpKind::Register, OpKind::Register)
            ),
            Self::MovmskGpr => movmsk_is_lowerable(instr),
            Self::CvtGprToXmm => {
                if instr.op0_kind() != OpKind::Register || !instr.op_register(0).is_xmm() {
                    return false;
                }
                match instr.op1_kind() {
                    OpKind::Register => true,
                    OpKind::Memory => mem_ea_ok(instr) && mem_size_ok_sse(instr),
                    _ => false,
                }
            }
            Self::CvtXmmToGpr => {
                if instr.op0_kind() != OpKind::Register {
                    return false;
                }
                if !matches!(instr.op_register(0).size(), 4 | 8) {
                    return false;
                }
                match instr.op1_kind() {
                    OpKind::Register => instr.op_register(1).is_xmm(),
                    OpKind::Memory => mem_ea_ok(instr) && mem_size_ok_sse(instr),
                    _ => false,
                }
            }
            Self::Shift => {
                if instr.op0_kind() != OpKind::Register || !instr.op_register(0).is_xmm() {
                    return false;
                }
                match instr.op1_kind() {
                    k if is_imm_kind(k) => true,
                    OpKind::Register => instr.op_register(1).is_xmm(),
                    OpKind::Memory => mem_ea_ok(instr) && mem_size_ok_sse(instr),
                    _ => false,
                }
            }
        }
    }
}

impl TermForm {
    /// Stable name of this terminator shape, for a refusal report.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Jmp => "term_jmp",
            Self::Jcc => "term_jcc",
            Self::Call => "term_call",
            Self::Ret => "term_ret",
        }
    }

    fn admits(self, instr: &Instruction) -> bool {
        match self {
            // `ret imm16` needs the interpreter (stack adjust after the pop).
            Self::Ret => instr.op_count() == 0,
            Self::Call => is_near_branch(instr) || is_call_rm64(instr),
            Self::Jmp | Self::Jcc => is_near_branch(instr),
        }
    }
}

fn is_near_branch(instr: &Instruction) -> bool {
    matches!(
        instr.op0_kind(),
        OpKind::NearBranch16 | OpKind::NearBranch32 | OpKind::NearBranch64
    )
}

/// `call r64` / `call [r/m64]`: a near indirect call whose pushed return
/// address is a full 64-bit `next_ip`.
///
/// This is the single largest interpreter residue on real guests (82.5 % of
/// 7-Zip's interpreted instructions, measured with `WIE_EXEC_TRACE=1`), because
/// every C++ virtual call, function pointer and import thunk goes through it —
/// and a block that *contains* one used to be refused wholesale, taking its
/// `mov` prologue to the interpreter with it.
///
/// In 64-bit mode a near call's operand size is forced to 64 bits, so the
/// `66 FF /2` form is *also* `call r64` (iced decodes it as `Call_rm64`; see
/// `dyn_call_tests::admission_of_indirect_calls_is_the_table_not_the_encoder`).
/// What stays on the interpreter is the **far** call (`FF /3`, a 16-byte
/// m16:64 pointer) — hence the 8-byte size check rather than an operand-kind
/// check — and `stack_pointer_increment` is the same delta the interpreter's
/// push assumes, so the two cannot disagree about the pushed width.
fn is_call_rm64(instr: &Instruction) -> bool {
    // iced reports the stack delta as a signed number: -8 for a 64-bit push,
    // -2 for the 16-bit operand-size form.
    if instr.stack_pointer_increment() != -8 {
        return false;
    }
    match instr.op0_kind() {
        OpKind::Register => instr.op_register(0).size() == 8,
        OpKind::Memory => instr.memory_size().size() == 8 && mem_ea_ok(instr),
        _ => false,
    }
}

/// `movsd` mnemonic is shared with string MOVS DWORD — only XMM forms are SSE.
fn sse_movsd_is_sse(instr: &Instruction) -> bool {
    instr.op0_register().is_xmm()
        || instr.op1_register().is_xmm()
        || matches!(
            instr.code(),
            iced_x86::Code::Movsd_xmm_xmmm64 | iced_x86::Code::Movsd_xmmm64_xmm
        )
}

/// `mov`/ALU shape: reg/reg, reg/imm, reg/mem, mem/reg, mem/imm (simple EA).
fn rm_imm_shape(instr: &Instruction) -> bool {
    let k0 = instr.op0_kind();
    let k1 = instr.op1_kind();
    match (k0, k1) {
        (OpKind::Register, OpKind::Register) => true,
        (OpKind::Register, imm) if is_imm_kind(imm) => true,
        (OpKind::Register, OpKind::Memory) | (OpKind::Memory, OpKind::Register) => {
            mem_ea_ok(instr) && mem_size_ok(instr)
        }
        (OpKind::Memory, imm) if is_imm_kind(imm) => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// One register-or-memory operand (`setcc`, `inc`/`dec`/`neg`/`not`).
fn unary_shape(instr: &Instruction) -> bool {
    match instr.op0_kind() {
        OpKind::Register => true,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// Dst register, src register or simple memory (`movzx`, `cmov`, `bsr`/`lzcnt`).
fn reg_rm_shape(instr: &Instruction) -> bool {
    if instr.op0_kind() != OpKind::Register {
        return false;
    }
    match instr.op1_kind() {
        OpKind::Register => true,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// `imul` 2-op (dst *= src) or 3-op (dst = src * imm). 1-op RDX:RAX → iced.
fn imul_is_lowerable(instr: &Instruction) -> bool {
    match instr.op_count() {
        2 => rm_imm_shape(instr),
        3 => {
            if instr.op0_kind() != OpKind::Register {
                return false;
            }
            let src_ok = match instr.op1_kind() {
                OpKind::Register => true,
                OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
                _ => false,
            };
            src_ok && is_imm_kind(instr.op2_kind())
        }
        _ => false,
    }
}

/// `div`/`idiv`: 32-bit operand in a register or simple memory operand.
///
/// 64-bit forms stay iced (RDX:RAX 128-bit dividend needs i128 helpers that
/// are not legalized on AArch64). 8/16-bit forms also stay iced.
fn div_is_lowerable(instr: &Instruction) -> bool {
    if instr.op_count() != 1 {
        return false;
    }
    match instr.op0_kind() {
        OpKind::Register => instr.op_register(0).size() == 4,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// Shift/rotate: dst reg or simple mem; count imm or CL.
fn shift_is_lowerable(instr: &Instruction) -> bool {
    let dst_ok = match instr.op0_kind() {
        OpKind::Register => true,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    };
    if !dst_ok {
        return false;
    }
    match instr.op1_kind() {
        k if is_imm_kind(k) => true,
        OpKind::Register => {
            // Only CL/RCX count forms in v1.
            matches!(
                instr.op_register(1),
                Register::CL | Register::CX | Register::ECX | Register::RCX
            )
        }
        _ => false,
    }
}

/// `push` r64 / imm / simple mem (64-bit stack ops only; 16-bit override → iced).
fn push_is_lowerable(instr: &Instruction) -> bool {
    match instr.op0_kind() {
        OpKind::Register => instr.op_register(0).size() != 2,
        imm if is_imm_kind(imm) => true,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// `pop` r64 / simple mem (not 16-bit override).
fn pop_is_lowerable(instr: &Instruction) -> bool {
    match instr.op0_kind() {
        OpKind::Register => instr.op_register(0).size() != 2,
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok(instr),
        _ => false,
    }
}

/// `lea` accepts every EA form; the operand at `op1` must be memory.
fn lea_is_simple(instr: &Instruction) -> bool {
    if instr.op0_kind() != OpKind::Register {
        return false;
    }
    if instr.op1_kind() != OpKind::Memory {
        return false;
    }
    mem_ea_ok(instr)
}

/// Dst xmm, src xmm or `m32`/`m64`/`m128`.
///
/// One body serves the packed, scalar, bitwise, compare and shuffle families:
/// the pre-ADR code had six separate functions with identical bodies plus an
/// unused `width` parameter.
fn xmm_rm_is_lowerable(instr: &Instruction) -> bool {
    if instr.op0_kind() != OpKind::Register || !instr.op_register(0).is_xmm() {
        return false;
    }
    match instr.op1_kind() {
        OpKind::Register => instr.op_register(1).is_xmm(),
        OpKind::Memory => mem_ea_ok(instr) && mem_size_ok_sse(instr),
        _ => false,
    }
}

/// `movq`: xmm↔xmm/m64, or xmm↔r64, or r64↔xmm.
fn sse_movq_is_lowerable(instr: &Instruction) -> bool {
    let k0 = instr.op0_kind();
    let k1 = instr.op1_kind();
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    match (k0, k1) {
        (OpKind::Register, OpKind::Register) => {
            // xmm ← xmm: fine.
            if r0.is_xmm() && r1.is_xmm() {
                return true;
            }
            // xmm ← gpr64: `lower_sse_movq` moves the low quadword and zeroes
            // the high 64 (MOVQ semantics) — lowerable since B4.
            if r0.is_xmm() && r1.size() == 8 {
                return true;
            }
            // gpr64 ← xmm: fine.
            r0.size() == 8 && r1.is_xmm()
        }
        (OpKind::Register, OpKind::Memory) => {
            (r0.is_xmm() || r0.size() == 8) && mem_ea_ok(instr) && mem_size_ok_sse(instr)
        }
        (OpKind::Memory, OpKind::Register) => {
            (r1.is_xmm() || r1.size() == 8) && mem_ea_ok(instr) && mem_size_ok_sse(instr)
        }
        _ => false,
    }
}

/// `movd`: xmm↔r32/m32.
fn sse_movd_is_lowerable(instr: &Instruction) -> bool {
    let k0 = instr.op0_kind();
    let k1 = instr.op1_kind();
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    match (k0, k1) {
        (OpKind::Register, OpKind::Register) => {
            (r0.is_xmm() && r1.size() == 4) || (r0.size() == 4 && r1.is_xmm())
        }
        (OpKind::Register, OpKind::Memory) => {
            r0.is_xmm() && mem_ea_ok(instr) && mem_size_ok_sse(instr)
        }
        (OpKind::Memory, OpKind::Register) => {
            r1.is_xmm() && mem_ea_ok(instr) && mem_size_ok_sse(instr)
        }
        _ => false,
    }
}

/// `movmskpd`/`movmskps`: dst GPR32/64, src xmm or `m128`.
///
/// op0 is a GPR, which is why this is not the `xmm_rm` shape.
fn movmsk_is_lowerable(instr: &Instruction) -> bool {
    instr.op0_kind() == OpKind::Register
        && (instr.op_register(0).is_gpr32() || instr.op_register(0).is_gpr64())
        && match instr.op1_kind() {
            OpKind::Register => instr.op_register(1).is_xmm(),
            OpKind::Memory => mem_ea_ok(instr) && mem_size_ok_sse(instr),
            _ => false,
        }
}

/// Memory sizes used by SSE loads/stores (4/8/16).
fn mem_size_ok_sse(instr: &Instruction) -> bool {
    let sz = instr.memory_size().size();
    matches!(sz, 4 | 8 | 16) || mem_size_ok(instr)
}

/// Whether *any* operand of `instr` is a memory reference.
///
/// Operand-agnostic on purpose. `XCHG` is symmetric, so the memory operand lands
/// in either ModRM field across the two encodings of the same instruction; iced
/// canonicalises both to op0, but relying on that is relying on a decoder
/// presentation detail rather than on the encoding. Scanning every operand also
/// means an instruction with a memory operand in a slot this code does not
/// enumerate (`LEA`'s op1, a three-operand form) is still classified correctly
/// rather than silently treated as register-only.
fn has_memory_operand(instr: &Instruction) -> bool {
    (0..instr.op_count()).any(|i| instr.op_kind(i) == OpKind::Memory)
}

/// Is this instruction's memory-operand form an **implicitly locked**
/// read-modify-write on x86-64?
///
/// This is the single source of truth for that architectural fact, and it is
/// deliberately *not* written in terms of `Instruction::has_lock_prefix()`.
///
/// ## Why the prefix byte cannot answer this
///
/// On x86-64 the memory forms of `XCHG` and `CMPXCHG` are implicitly locked by
/// the architecture whether or not a `F0` prefix byte is present, and compilers
/// emit them **without** one as the norm (`cmpxchg [rbx], rcx` is `0F B1 /r`).
/// iced, like every x86 decoder, reports only the *prefix byte*, so
/// `has_lock_prefix()` is `false` for the standard encoding of the two
/// instructions whose memory form most needs to be atomic. Keying the lowerability
/// decision on it compiled a guest's 64-bit CAS — every `std::atomic` CAS and
/// every `InterlockedCompareExchange64` — into a plain load plus store while the
/// interpreter performed a real atomic CAS, i.e. no mutual exclusion at all on the
/// default backend.
///
/// `XCHG` and `CMPXCHG` are the *only* two instructions in the ISA with an
/// implicitly locked memory form. Notably **`INC`/`DEC`/`NOT`/`NEG` on memory are
/// not**: an unlocked `inc [rbx]` is a plain read-modify-write in hardware too, so
/// admitting them is architecturally correct and rejecting them would cost
/// coverage for nothing. Anything added to this list changes guest-visible atomic
/// behaviour, so it belongs here and nowhere else.
fn mem_rmw_is_implicitly_locked(instr: &Instruction) -> bool {
    has_memory_operand(instr) && matches!(instr.mnemonic(), Mnemonic::Xchg | Mnemonic::Cmpxchg)
}

/// Can this memory-operand read-modify-write be lowered as a plain load plus
/// store?
///
/// Every decision that depends on memory-RMW atomicity routes through here, so
/// the rule lives in exactly one place: a form is refused when the architecture
/// guarantees atomicity (see [`mem_rmw_is_implicitly_locked`]) **or** when the
/// encoding carries an explicit `LOCK`. The JIT emits no `atomic_rmw`, so both
/// cases fall back to the interpreter, whose `crate::exec` `XCHG` / `CMPXCHG` /
/// locked `XADD` / locked `BTX` are real host atomics.
///
/// Do not re-admit a refused form without a native atomic lowering: the old
/// `call_load` + `call_store` pair silently compiled a guest spinlock down to an
/// unsynchronised load/store pair.
fn mem_rmw_is_lowerable(instr: &Instruction) -> bool {
    if !has_memory_operand(instr) {
        return true; // register-only: no memory ordering to provide
    }
    // Explicit LOCK: iced *does* report this prefix byte faithfully, so it is the
    // right test for the mnemonics that are not implicitly locked — which is the
    // ALU and unary groups, and the only `LOCK`-able forms left. (Not the
    // shift/rotate group: Intel SDM Vol. 2D's LOCK-able set omits `SHL`/`SHR`/
    // `SAR`/`ROL`/`ROR`/`RCL`/`RCR`, so iced reports every `F0`-prefixed encoding
    // of them as `Code::INVALID` and no guest can emit one. `mem_rmw_is_lowerable`
    // is still applied to them, which costs nothing and keeps a future decoder
    // relaxation from admitting a non-atomic shift.) The interpreter executes the
    // ALU/unary locked forms through `atomic_rmw` (`is_locked_mem_rmw` in `exec`),
    // so refusing them here hands the guest a real atomic RMW instead of a
    // non-atomic load plus store. A bare `add [rbx], eax` is unaffected: it is not
    // implicitly locked and carries no prefix, so it still compiles.
    !(instr.has_lock_prefix() || mem_rmw_is_implicitly_locked(instr))
}

/// EA: optional base + optional index with scale 1/2/4/8 (no segment tricks).
fn mem_ea_ok(instr: &Instruction) -> bool {
    let index = instr.memory_index();
    if index == Register::None {
        return true;
    }
    matches!(instr.memory_index_scale(), 1 | 2 | 4 | 8)
}

fn mem_size_ok(instr: &Instruction) -> bool {
    matches!(
        instr.memory_size(),
        MemorySize::UInt8
            | MemorySize::Int8
            | MemorySize::UInt16
            | MemorySize::Int16
            | MemorySize::UInt32
            | MemorySize::Int32
            | MemorySize::UInt64
            | MemorySize::Int64
            | MemorySize::QwordOffset
            | MemorySize::SegPtr64
    ) || {
        // Fallback: register peer size 1/2/4/8.
        if instr.op0_kind() == OpKind::Register {
            matches!(instr.op_register(0).size(), 1 | 2 | 4 | 8)
        } else if instr.op1_kind() == OpKind::Register {
            matches!(instr.op_register(1).size(), 1 | 2 | 4 | 8)
        } else {
            false
        }
    }
}
