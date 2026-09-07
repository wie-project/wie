//! Instruction execution for the iced x86-64 interpreter.
//!
//! Entry points for the CPU backends: [`step`] / [`execute_one`], the
//! stop-bitmap [`HookWindow`], and the shared operand / control-flow helpers.
//! The decode cache, instruction-class enums, GPR arithmetic, SSE/FP, and REP
//! string execution live in the sibling submodules below.
//!
//! Low-level CPU arithmetic intentionally uses wrapping ops, truncating casts,
//! and direct indexing of fixed-size buffers — clippy pedantic is not useful here.

mod cache;
mod gpr;
mod ops;
mod sse;
mod sse_types;
mod string;
mod x87;

use crate::CpuError;
use crate::consts::{DWORD_BYTES, QWORD_BITS, SHIFT_MASK_32, SHIFT_MASK_64, XMM_BYTES};
use crate::mem::GuestMemory;
use crate::regs::{self, RegFile, Rflags};
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use cache::{ICED_COUNTERS, ICED_TRACE_ENABLED, decode_at};
use gpr::{
    exec_arith, exec_bit, exec_bswap, exec_cmov, exec_div, exec_imul, exec_lea, exec_mov,
    exec_movzx, exec_mul, exec_pop, exec_push, exec_setcc,
};
use ops::{ArithOp, BitOp, ShiftKind, cond_from};
use sse::{
    exec_sse_bitwise, exec_sse_byte_shift, exec_sse_cmp_fp, exec_sse_comis, exec_sse_cvt_fp_to_gpr,
    exec_sse_cvt_gpr_to_fp, exec_sse_cvt_packed, exec_sse_cvtdq2pd, exec_sse_cvtpd2dq,
    exec_sse_cvtpd2ps, exec_sse_cvtps2pd, exec_sse_cvtsd2ss, exec_sse_cvtss2sd, exec_sse_cvttpd2dq,
    exec_sse_int_binop, exec_sse_minmax_packed, exec_sse_minmax_scalar, exec_sse_mov,
    exec_sse_movd, exec_sse_movhlps, exec_sse_movhps, exec_sse_movlpd, exec_sse_movmsk,
    exec_sse_movq, exec_sse_packed_fp, exec_sse_pmovmskb, exec_sse_psadbw, exec_sse_pshufb,
    exec_sse_pshufd, exec_sse_pshuflw_hw, exec_sse_punpck, exec_sse_punpck_lanes,
    exec_sse_rcp_rsqrt, exec_sse_scalar_fp, exec_sse_shift, exec_sse_shufpd, exec_sse_sqrt_packed,
    exec_sse_sqrt_scalar, is_sse_movsd, sse_int_op, sse_shift_op,
};
use sse_types::{FpOp, SseBitOp};
use string::{exec_cmps, exec_lods, exec_movs, exec_scas, exec_stos};

// Re-exports preserving the old `exec::` surface for iced_cpu.rs / jit / lib.rs.
// `iced_decode_cache_flush` is only called from tests today, so the re-export
// is dead in lib builds (matches the original `#[allow(dead_code)]`).
#[allow(unused_imports)]
pub(crate) use cache::iced_decode_cache_flush;
pub(crate) use sse::{
    sse_cvt, sse_fp_binop, sse_fp_unop, sse_int_binop_half, sse_pshufb_hi, sse_pshufb_lo,
    sse_shift_half,
};
pub(crate) use sse_types::{SseCvtOp, SseFpBinOp, SseFpUnOp, SseIntOp, SseShiftOp};
pub(crate) use string::{RepPrefix, StringOpKind, run_string_op};

/// Access kind for invalid-memory reporting.
///
/// Encodes to the Unicorn-ish numeric codes consumed by the host
/// ([`crate::InvalidMemoryAccess::access_type`]): 0 = read, 1 = write, 16 = fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessType {
    Read,
    Write,
    Fetch,
}

impl AccessType {
    /// Numeric code passed to host memory checks (must match Unicorn encoding).
    #[must_use]
    pub(crate) const fn as_i32(self) -> i32 {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Fetch => 16,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvalidMem {
    pub access_type: AccessType,
    pub address: u64,
    pub size: i32,
    pub value: i64,
}

#[derive(Debug)]
pub(crate) enum StepResult {
    /// Advanced RIP (or branch) normally.
    Continue,
    /// Hit a host-stop hook address (caller should not execute).
    HostStop { address: u64, size: u32 },
    /// Invalid memory during this step.
    InvalidMemory(InvalidMem),
}

// ── Wave 4: degrade-not-die fallback ─────────────────────────────────────
//
// An unimplemented mnemonic no longer stops the session. The fallback
// executes the instruction PARTIALLY: RIP advances past it and register /
// memory state is left untouched (the documented approximation — a guest
// sees a no-op where real silicon would have produced a value). The first
// occurrence of each mnemonic is traced once, every occurrence is counted
// (the Wave 4 coverage metric surfaces it in the profile report), and
// `WIE_DEGRADE=0` restores the hard stop for bisect.

/// Decode + execute one instruction at `regs.rip`.
///
/// Lightweight tracer: the first time each non-JIT mnemonic is interpreted,
/// it is logged once at `info` level (opt-in: `WIE_EXEC_TRACE=1`).  Run with
/// `WIE_EXEC_TRACE=1 7za …` to discover which instructions keep code in the
/// interpreter rather than the JIT.
/// Degrade-not-die gate (default on; `WIE_DEGRADE=0`/`false`/`off` disables).
static DEGRADE_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    !matches!(
        std::env::var("WIE_DEGRADE"),
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
    )
});

/// Total fallback-executed instructions (the Wave 4 coverage metric).
static DEGRADED_INSNS: AtomicU64 = AtomicU64::new(0);
/// Mnemonic debug names already traced (first occurrence per mnemonic).
static DEGRADED_SEEN: LazyLock<Mutex<ahash::HashSet<u32>>> = LazyLock::new(|| {
    use ahash::HashSetExt;
    Mutex::new(ahash::HashSet::new())
});

/// Total degrade-fallback executions, for the runtime profile report.
#[must_use]
pub fn degraded_insn_count() -> u64 {
    DEGRADED_INSNS.load(Ordering::Relaxed)
}

type DegradedSeen = ahash::HashSet<u32>;

fn degrade_locked_seen() -> MutexGuard<'static, DegradedSeen> {
    match DEGRADED_SEEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn step(
    mem: &GuestMemory,
    regs: &mut RegFile,
    hook: Option<&HookWindow>,
) -> Result<StepResult, CpuError> {
    let rip = regs.rip;

    if let Some(h) = hook
        && h.should_host_stop(rip)
    {
        // Decode first for accurate size when possible (cache-backed).
        let size = decode_at(mem, rip).map_or(1, |(_, len)| len);
        return Ok(StepResult::HostStop { address: rip, size });
    }

    let Some((instr, _len)) = decode_at(mem, rip) else {
        // Distinguish "unmapped fetch" from "invalid encoding" by re-probing
        // `fetch_into` so callers see the same InvalidMem vs error split as
        // before the cache was introduced.
        let mut probe = [0_u8; 1];
        if mem.fetch_into(rip, &mut probe).is_err() {
            return Ok(StepResult::InvalidMemory(InvalidMem {
                access_type: AccessType::Fetch,
                address: rip,
                size: 1,
                value: 0,
            }));
        }
        return Err(CpuError::Message(format!(
            "invalid instruction at {rip:#x}"
        )));
    };

    // Tracer: count how many times each mnemonic hits the interpreter.
    // Activated by WIE_EXEC_TRACE=1 (release + debug).
    if *ICED_TRACE_ENABLED {
        let m = instr.mnemonic() as usize;
        if m < ICED_COUNTERS.len() {
            ICED_COUNTERS[m].fetch_add(1, Ordering::Relaxed);
        }
    }

    let next_ip = instr.next_ip();
    // Do not advance RIP until the instruction completes successfully.
    // (Faults must leave RIP at the faulting instruction — Unicorn semantics.)
    match execute_one(mem, regs, &instr, next_ip) {
        Ok(()) => Ok(StepResult::Continue),
        Err(StepExecError::InvalidMemory(inv)) => {
            // Ensure RIP still points at the faulting insn.
            regs.rip = rip;
            Ok(StepResult::InvalidMemory(inv))
        }
        Err(StepExecError::Cpu(e)) => {
            regs.rip = rip;
            Err(e)
        }
    }
}

#[derive(Debug)]
pub(crate) enum StepExecError {
    InvalidMemory(InvalidMem),
    Cpu(CpuError),
}

impl From<CpuError> for StepExecError {
    fn from(e: CpuError) -> Self {
        Self::Cpu(e)
    }
}

/// Hook window + stop bitmap (1 = host stop).
///
/// Bitmap is immutable after `install_runtime_hooks` — wrap as `Arc<[u8]>`
/// so cloning per worker/JIT thread is a refcount bump instead of a full
/// Vec copy (was material at spawn time for large fake-API ranges).
#[derive(Debug, Clone)]
pub(crate) struct HookWindow {
    pub begin: u64,
    pub end: u64,
    pub stop_bitmap: std::sync::Arc<[u8]>,
}

impl HookWindow {
    #[must_use]
    pub(crate) fn should_host_stop(&self, address: u64) -> bool {
        if self.stop_bitmap.is_empty() {
            return address >= self.begin && address <= self.end;
        }
        if address < self.begin {
            return false;
        }
        let range_len = self.end.saturating_sub(self.begin).saturating_add(1);
        let offset = address.saturating_sub(self.begin);
        if offset >= range_len {
            return false;
        }
        let bit_index = usize::try_from(offset).unwrap_or(usize::MAX);
        let byte_index = bit_index / 8;
        let bit = bit_index % 8;
        match self.stop_bitmap.get(byte_index) {
            Some(&byte) => (byte & (1_u8 << bit)) != 0,
            None => true,
        }
    }
}

fn execute_one(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    next_ip: u64,
) -> Result<(), StepExecError> {
    // Fall-through RIP; branches / call / ret override. Set only after we know
    // the op will not fault on decode of operands — still set early for LEA/jcc
    // that need next_ip; memory ops that fault restore RIP in `step`.
    regs.rip = next_ip;

    match instr.mnemonic() {
        // No-ops / PE userspace I/O stubs (no real ports).
        Mnemonic::Nop
        | Mnemonic::Fnclex
        | Mnemonic::Fninit
        | Mnemonic::Finit
        | Mnemonic::Endbr64
        | Mnemonic::Endbr32
        | Mnemonic::Out
        | Mnemonic::Outsb
        | Mnemonic::Outsw
        | Mnemonic::Outsd
        // PAUSE (F3 90): spin-wait hint used inside CRT / std::mutex / spinlocks
        // when the compiler emits contention-friendly busy-waits. On real hardware
        // it hints the pipeline to pause; semantically it's a no-op. Failure mode
        // before this stub was intermittent worker crashes in `cpp_threads` when
        // the CRT lock happened to spin (see also the JIT `Mnemonic::Pause` lower).
        | Mnemonic::Pause
        // Prefetch hints (0F 18 /0-/3, 0F 0D /1, 0F 0D /2): cache hints with no
        // architectural effect — SDL2's memcpy paths emit them liberally.
        | Mnemonic::Prefetchnta
        | Mnemonic::Prefetcht0
        | Mnemonic::Prefetcht1
        | Mnemonic::Prefetcht2
        | Mnemonic::Prefetchw
        | Mnemonic::Prefetchwt1 => Ok(()),

        Mnemonic::Mov => exec_mov(mem, regs, instr),
        Mnemonic::Movzx => exec_movzx(mem, regs, instr, false),
        Mnemonic::Movsx | Mnemonic::Movsxd => exec_movzx(mem, regs, instr, true),
        Mnemonic::Lea => exec_lea(regs, instr),

        Mnemonic::Push => exec_push(mem, regs, instr),
        Mnemonic::Pop => exec_pop(mem, regs, instr),

        Mnemonic::Add => exec_arith(mem, regs, instr, ArithOp::Add),
        Mnemonic::Adc => exec_arith(mem, regs, instr, ArithOp::Adc),
        Mnemonic::Sub => exec_arith(mem, regs, instr, ArithOp::Sub),
        Mnemonic::Sbb => exec_arith(mem, regs, instr, ArithOp::Sbb),
        Mnemonic::Xor => exec_arith(mem, regs, instr, ArithOp::Xor),
        Mnemonic::Or => exec_arith(mem, regs, instr, ArithOp::Or),
        Mnemonic::And => exec_arith(mem, regs, instr, ArithOp::And),
        Mnemonic::Cmp => exec_arith(mem, regs, instr, ArithOp::Cmp),
        Mnemonic::Test => exec_test(mem, regs, instr),
        Mnemonic::Inc => exec_inc_dec(mem, regs, instr, true),
        Mnemonic::Dec => exec_inc_dec(mem, regs, instr, false),
        Mnemonic::Neg => exec_neg(mem, regs, instr),
        Mnemonic::Not => exec_not(mem, regs, instr),
        Mnemonic::Imul => exec_imul(mem, regs, instr),
        Mnemonic::Mul => exec_mul(mem, regs, instr),
        Mnemonic::Div => exec_div(mem, regs, instr, false),
        Mnemonic::Idiv => exec_div(mem, regs, instr, true),
        Mnemonic::Lzcnt => exec_lzcnt(mem, regs, instr),
        // Bsr/Bsf: bit scan reverse / forward — ZF = (src == 0), dst undefined
        // on zero (written 0). The JIT lowers Bsr; iced needs both for
        // fallback blocks (mingw SDL2's video init uses Bsr).
        Mnemonic::Bsr => exec_bit_scan(mem, regs, instr, true),
        Mnemonic::Bsf => exec_bit_scan(mem, regs, instr, false),

        Mnemonic::Shl | Mnemonic::Sal => exec_shift(mem, regs, instr, ShiftKind::Shl),
        Mnemonic::Shr => exec_shift(mem, regs, instr, ShiftKind::Shr),
        Mnemonic::Sar => exec_shift(mem, regs, instr, ShiftKind::Sar),
        Mnemonic::Rol => exec_shift(mem, regs, instr, ShiftKind::Rol),
        Mnemonic::Ror => exec_shift(mem, regs, instr, ShiftKind::Ror),
        Mnemonic::Rcl => exec_shift(mem, regs, instr, ShiftKind::Rcl),
        Mnemonic::Rcr => exec_shift(mem, regs, instr, ShiftKind::Rcr),

        Mnemonic::Jmp => exec_jmp(mem, regs, instr),
        Mnemonic::Call => exec_call(mem, regs, instr, next_ip),
        Mnemonic::Ret => exec_ret(mem, regs, instr),

        // iced uses one primary name per condition (Je not Jz, etc.).
        m @ (Mnemonic::Je
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
        | Mnemonic::Jnp) => {
            exec_jcc(regs, instr, cond_from(m, regs));
            Ok(())
        }

        m @ (Mnemonic::Cmove
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
        | Mnemonic::Cmovnp) => exec_cmov(mem, regs, instr, cond_from(m, regs)),

        m @ (Mnemonic::Sete
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
        | Mnemonic::Setnp) => exec_setcc(mem, regs, instr, cond_from(m, regs)),

        Mnemonic::Xchg => exec_xchg(mem, regs, instr),
        Mnemonic::Xadd => exec_xadd(mem, regs, instr),
        Mnemonic::Cmpxchg => exec_cmpxchg(mem, regs, instr),
        Mnemonic::Bswap => exec_bswap(regs, instr),
        Mnemonic::Bt => exec_bit(mem, regs, instr, BitOp::Bt),
        Mnemonic::Bts => exec_bit(mem, regs, instr, BitOp::Bts),
        Mnemonic::Btr => exec_bit(mem, regs, instr, BitOp::Btr),
        Mnemonic::Btc => exec_bit(mem, regs, instr, BitOp::Btc),

        Mnemonic::Cdqe => {
            let eax = regs.rax() as i32;
            regs.set_rax(i64::from(eax) as u64);
            Ok(())
        }
        Mnemonic::Cwde => {
            let ax = regs.rax() as i16;
            regs.write_reg(Register::EAX, i64::from(ax) as u64 & 0xffff_ffff)?;
            Ok(())
        }
        Mnemonic::Cbw => {
            let al = regs.rax() as i8;
            regs.write_reg(Register::AX, i64::from(al) as u64 & 0xffff)?;
            Ok(())
        }
        Mnemonic::Cdq => {
            let eax = regs.rax() as i32;
            regs.set_rdx(if eax < 0 { 0xffff_ffff } else { 0 });
            Ok(())
        }
        Mnemonic::Cwd => {
            let ax = regs.rax() as i16;
            regs.write_reg(Register::DX, if ax < 0 { 0xffff } else { 0 })?;
            Ok(())
        }
        Mnemonic::Cqo => {
            let rax = regs.rax() as i64;
            regs.set_rdx(if rax < 0 { u64::MAX } else { 0 });
            Ok(())
        }
        Mnemonic::Cld => {
            regs.set_flag(Rflags::DF, false);
            Ok(())
        }
        Mnemonic::Std => {
            regs.set_flag(Rflags::DF, true);
            Ok(())
        }
        Mnemonic::Clc => {
            regs.set_flag(Rflags::CF, false);
            Ok(())
        }
        Mnemonic::Stc => {
            regs.set_flag(Rflags::CF, true);
            Ok(())
        }
        Mnemonic::Cmc => {
            regs.set_flag(Rflags::CF, !regs.flag(Rflags::CF));
            Ok(())
        }
        Mnemonic::Pushfq => {
            push_n(mem, regs, u64::from(regs.rflags), 8)?;
            Ok(())
        }
        Mnemonic::Popfq => {
            let v = pop_n(mem, regs, 8)?;
            // Keep reserved bit 1 set.
            regs.rflags = Rflags::from((v & !u64::from(Rflags::ALWAYS1)) | u64::from(Rflags::ALWAYS1));
            Ok(())
        }
        Mnemonic::Leave => {
            regs.set_rsp(regs.rbp());
            let val = pop64(mem, regs)?;
            regs.set_rbp(val);
            Ok(())
        }

        Mnemonic::Stosb => exec_stos(mem, regs, instr, 1),
        Mnemonic::Stosw => exec_stos(mem, regs, instr, 2),
        Mnemonic::Stosd => exec_stos(mem, regs, instr, 4),
        Mnemonic::Stosq => exec_stos(mem, regs, instr, 8),
        Mnemonic::Movsb => exec_movs(mem, regs, instr, 1),
        Mnemonic::Movsw => exec_movs(mem, regs, instr, 2),
        // Movsd is both string (A5) and SSE2 scalar — disambiguate by XMM use.
        Mnemonic::Movsd => {
            if is_sse_movsd(instr) {
                exec_sse_mov(mem, regs, instr, 8, true)
            } else {
                exec_movs(mem, regs, instr, 4)
            }
        }
        Mnemonic::Movsq => exec_movs(mem, regs, instr, 8),
        Mnemonic::Lodsb => exec_lods(mem, regs, instr, 1),
        Mnemonic::Lodsd => exec_lods(mem, regs, instr, 4),
        Mnemonic::Lodsq => exec_lods(mem, regs, instr, 8),
        Mnemonic::Scasb => exec_scas(mem, regs, instr, 1),
        Mnemonic::Scasw => exec_scas(mem, regs, instr, 2),
        Mnemonic::Scasd => exec_scas(mem, regs, instr, 4),
        Mnemonic::Scasq => exec_scas(mem, regs, instr, 8),
        Mnemonic::Cmpsb => exec_cmps(mem, regs, instr, 1),
        Mnemonic::Cmpsw => exec_cmps(mem, regs, instr, 2),
        Mnemonic::Cmpsq => exec_cmps(mem, regs, instr, 8),

        // Scalar / packed SSE moves (enough for CRT / memcpy helpers).
        Mnemonic::Movss => exec_sse_mov(mem, regs, instr, 4, true),
        Mnemonic::Movaps
        | Mnemonic::Movups
        | Mnemonic::Movdqa
        | Mnemonic::Movdqu
        | Mnemonic::Movapd
        | Mnemonic::Movupd => exec_sse_mov(mem, regs, instr, XMM_BYTES, false),
        Mnemonic::Movq => exec_sse_movq(mem, regs, instr),
        Mnemonic::Pmovmskb | Mnemonic::Vpmovmskb => exec_sse_pmovmskb(regs, instr),
        Mnemonic::Xorps | Mnemonic::Xorpd | Mnemonic::Pxor => {
            exec_sse_bitwise(mem, regs, instr, SseBitOp::Xor)
        }
        Mnemonic::Andps | Mnemonic::Andpd | Mnemonic::Pand => {
            exec_sse_bitwise(mem, regs, instr, SseBitOp::And)
        }
        Mnemonic::Orps | Mnemonic::Orpd | Mnemonic::Por => {
            exec_sse_bitwise(mem, regs, instr, SseBitOp::Or)
        }
        Mnemonic::Andnps | Mnemonic::Andnpd | Mnemonic::Pandn => {
            exec_sse_bitwise(mem, regs, instr, SseBitOp::Andn)
        }
        Mnemonic::Movd => exec_sse_movd(mem, regs, instr),
        // Move packed floats between XMM upper/lower halves and memory.
        // MOVHPD (66-prefixed) shares MOVHPS semantics for its two legal
        // memory forms; same for MOVLPS/MOVLPD on the low half.
        Mnemonic::Movhps | Mnemonic::Movhpd => exec_sse_movhps(mem, regs, instr),
        Mnemonic::Movlps | Mnemonic::Movlpd => exec_sse_movlpd(mem, regs, instr),
        Mnemonic::Movhlps | Mnemonic::Movlhps => exec_sse_movhlps(regs, instr),
        // SSE2 unpack / shuffle / compare.
        Mnemonic::Punpcklqdq
        | Mnemonic::Punpckhqdq
        | Mnemonic::Unpcklpd
        | Mnemonic::Unpckhpd => exec_sse_punpck(regs, instr),
        Mnemonic::Punpcklbw
        | Mnemonic::Punpcklwd
        | Mnemonic::Punpckldq
        | Mnemonic::Punpckhbw
        | Mnemonic::Punpckhwd
        | Mnemonic::Punpckhdq => exec_sse_punpck_lanes(mem, regs, instr),
        Mnemonic::Pshufd => exec_sse_pshufd(mem, regs, instr),
        Mnemonic::Pshuflw | Mnemonic::Pshufhw => exec_sse_pshuflw_hw(mem, regs, instr),
        Mnemonic::Pshufb => exec_sse_pshufb(mem, regs, instr),
        // SHUFPD shuffles 64-bit lanes between two sources (the UCRT wcscpy
        // fast path and similar 128-bit copies use it).
        Mnemonic::Shufpd => exec_sse_shufpd(mem, regs, instr),
        // Packed integer arithmetic / compare / pack (SSE2 integer family).
        Mnemonic::Paddb
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
        | Mnemonic::Packuswb => exec_sse_int_binop(mem, regs, instr, sse_int_op(instr.mnemonic())),
        // Packed shifts: imm8 or variable XMM count.
        Mnemonic::Psllw
        | Mnemonic::Pslld
        | Mnemonic::Psllq
        | Mnemonic::Psrlw
        | Mnemonic::Psrld
        | Mnemonic::Psrlq
        | Mnemonic::Psraw
        | Mnemonic::Psrad => exec_sse_shift(mem, regs, instr, sse_shift_op(instr.mnemonic())),
        // Whole-XMM byte shifts (66 0F 73 /3 ib and /7 ib; imm8 count only).
        Mnemonic::Psrldq | Mnemonic::Pslldq => exec_sse_byte_shift(regs, instr),
        Mnemonic::Psadbw => exec_sse_psadbw(mem, regs, instr),
        // FP sqrt / min / max (scalar + packed).
        Mnemonic::Sqrtss => exec_sse_sqrt_scalar(mem, regs, instr, false),
        Mnemonic::Sqrtsd => exec_sse_sqrt_scalar(mem, regs, instr, true),
        Mnemonic::Sqrtps => exec_sse_sqrt_packed(mem, regs, instr, false),
        Mnemonic::Sqrtpd => exec_sse_sqrt_packed(mem, regs, instr, true),
        Mnemonic::Minss => exec_sse_minmax_scalar(mem, regs, instr, SseFpBinOp::Minss),
        Mnemonic::Maxss => exec_sse_minmax_scalar(mem, regs, instr, SseFpBinOp::Maxss),
        Mnemonic::Minsd => exec_sse_minmax_scalar(mem, regs, instr, SseFpBinOp::Minsd),
        Mnemonic::Maxsd => exec_sse_minmax_scalar(mem, regs, instr, SseFpBinOp::Maxsd),
        Mnemonic::Minps => exec_sse_minmax_packed(mem, regs, instr, SseFpBinOp::Minps),
        Mnemonic::Maxps => exec_sse_minmax_packed(mem, regs, instr, SseFpBinOp::Maxps),
        Mnemonic::Minpd => exec_sse_minmax_packed(mem, regs, instr, SseFpBinOp::Minpd),
        Mnemonic::Maxpd => exec_sse_minmax_packed(mem, regs, instr, SseFpBinOp::Maxpd),
        // FP compare → RFLAGS.
        Mnemonic::Comiss | Mnemonic::Ucomiss => exec_sse_comis(mem, regs, instr, false),
        Mnemonic::Comisd | Mnemonic::Ucomisd => exec_sse_comis(mem, regs, instr, true),
        // FP compare → lane masks. CMPSD is ambiguous with the string op:
        // the SSE form carries an imm8 predicate (3 operands); the string
        // form has implicit EDI/ESI operands and no immediate.
        Mnemonic::Cmppd => exec_sse_cmp_fp(mem, regs, instr, true, true),
        Mnemonic::Cmpps => exec_sse_cmp_fp(mem, regs, instr, true, false),
        Mnemonic::Cmpss => exec_sse_cmp_fp(mem, regs, instr, false, false),
        Mnemonic::Cmpsd if instr.op_count() >= 3 => {
            exec_sse_cmp_fp(mem, regs, instr, false, true)
        }
        // Sign-mask extraction.
        Mnemonic::Movmskpd => exec_sse_movmsk(mem, regs, instr, true),
        Mnemonic::Movmskps => exec_sse_movmsk(mem, regs, instr, false),
        // Reciprocal / reciprocal-sqrt (packed + scalar).
        Mnemonic::Rcpps => exec_sse_rcp_rsqrt(mem, regs, instr, true, false),
        Mnemonic::Rcpss => exec_sse_rcp_rsqrt(mem, regs, instr, false, false),
        Mnemonic::Rsqrtps => exec_sse_rcp_rsqrt(mem, regs, instr, true, true),
        Mnemonic::Rsqrtss => exec_sse_rcp_rsqrt(mem, regs, instr, false, true),
        Mnemonic::Cmpsd => exec_cmps(mem, regs, instr, 4),
        // Integer ↔ FP converts.
        Mnemonic::Cvtsi2ss | Mnemonic::Cvtsi2sd => exec_sse_cvt_gpr_to_fp(mem, regs, instr),
        Mnemonic::Cvttss2si
        | Mnemonic::Cvtss2si
        | Mnemonic::Cvttsd2si
        | Mnemonic::Cvtsd2si => exec_sse_cvt_fp_to_gpr(mem, regs, instr),
        Mnemonic::Cvtps2dq | Mnemonic::Cvtdq2ps | Mnemonic::Cvttps2dq => {
            exec_sse_cvt_packed(mem, regs, instr)
        }
        // CVTDQ2PD: two packed dwords (low 64 bits) → two packed doubles.
        Mnemonic::Cvtdq2pd => exec_sse_cvtdq2pd(mem, regs, instr),
        // CVTPS2PD: two packed singles (low 64 bits) → two packed doubles.
        Mnemonic::Cvtps2pd => exec_sse_cvtps2pd(mem, regs, instr),
        // CVTPD2DQ / CVTPD2PS: two packed doubles → dwords / singles.
        Mnemonic::Cvtpd2dq => exec_sse_cvtpd2dq(mem, regs, instr),
        Mnemonic::Cvtpd2ps => exec_sse_cvtpd2ps(mem, regs, instr),
        Mnemonic::Cvttpd2dq => exec_sse_cvttpd2dq(mem, regs, instr),
        // Scalar converts: low lane only, upper destination bits preserved.
        Mnemonic::Cvtsd2ss => exec_sse_cvtsd2ss(mem, regs, instr),
        Mnemonic::Cvtss2sd => exec_sse_cvtss2sd(mem, regs, instr),
        // STMXCSR/LDMXCSR: store/load the MXCSR control register to/from m32.
        // FP semantics use the native ARM64 rounding (default = nearest, which
        // matches the x86 reset value), so only the stored word is tracked.
        Mnemonic::Stmxcsr => {
            let addr = effective_address(regs, instr)?;
            write_mem_value(mem, addr, u64::from(regs.mxcsr()), DWORD_BYTES)?;
            Ok(())
        }
        Mnemonic::Ldmxcsr => {
            let addr = effective_address(regs, instr)?;
            let value = read_mem_value(mem, addr, DWORD_BYTES)?;
            let word = u32::try_from(value)
                .map_err(|_| StepExecError::Cpu(CpuError::Message("ldmxcsr value".into())))?;
            regs.set_mxcsr(word);
            Ok(())
        }
        Mnemonic::Addss => exec_sse_scalar_fp(mem, regs, instr, FpOp::Add, false),
        Mnemonic::Subss => exec_sse_scalar_fp(mem, regs, instr, FpOp::Sub, false),
        Mnemonic::Mulss => exec_sse_scalar_fp(mem, regs, instr, FpOp::Mul, false),
        Mnemonic::Divss => exec_sse_scalar_fp(mem, regs, instr, FpOp::Div, false),
        Mnemonic::Addsd => exec_sse_scalar_fp(mem, regs, instr, FpOp::Add, true),
        Mnemonic::Subsd => exec_sse_scalar_fp(mem, regs, instr, FpOp::Sub, true),
        Mnemonic::Mulsd => exec_sse_scalar_fp(mem, regs, instr, FpOp::Mul, true),
        Mnemonic::Divsd => exec_sse_scalar_fp(mem, regs, instr, FpOp::Div, true),
        Mnemonic::Addps => exec_sse_packed_fp(mem, regs, instr, FpOp::Add, false),
        Mnemonic::Subps => exec_sse_packed_fp(mem, regs, instr, FpOp::Sub, false),
        Mnemonic::Mulps => exec_sse_packed_fp(mem, regs, instr, FpOp::Mul, false),
        Mnemonic::Divps => exec_sse_packed_fp(mem, regs, instr, FpOp::Div, false),
        Mnemonic::Addpd => exec_sse_packed_fp(mem, regs, instr, FpOp::Add, true),
        Mnemonic::Subpd => exec_sse_packed_fp(mem, regs, instr, FpOp::Sub, true),
        Mnemonic::Mulpd => exec_sse_packed_fp(mem, regs, instr, FpOp::Mul, true),
        Mnemonic::Divpd => exec_sse_packed_fp(mem, regs, instr, FpOp::Div, true),

        // Minimal stubs: enough for CRT init that queries the host.
        Mnemonic::Cpuid => {
            // Leaf in EAX; return zeros (guest rarely depends on exact bits at PE entry).
            regs.set_rax(0);
            regs.set_gpr(3, 0); // RBX
            regs.set_rcx(0);
            regs.set_rdx(0);
            Ok(())
        }
        Mnemonic::Rdtsc => {
            // Monotonic-ish host time; not architectural.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            regs.set_rax(t & 0xffff_ffff);
            regs.set_rdx(t >> 32);
            Ok(())
        }
        // PE userspace: no real I/O ports — zero reads.
        Mnemonic::In => {
            regs.set_rax(0);
            Ok(())
        }
        Mnemonic::Insb | Mnemonic::Insw | Mnemonic::Insd => {
            // REP IN* rarely used; ignore.
            if instr.has_rep_prefix() {
                regs.set_rcx(0);
            }
            Ok(())
        }

        // Wave 4 x87 subset: the scalar stack ops; anything exec_x87 does not
        // implement flows into the same degrade-not-die fallback.
        Mnemonic::Fld
        | Mnemonic::Fldz
        | Mnemonic::Fld1
        | Mnemonic::Fst
        | Mnemonic::Fstp
        | Mnemonic::Fild
        | Mnemonic::Fist
        | Mnemonic::Fistp
        | Mnemonic::Fadd
        | Mnemonic::Faddp
        | Mnemonic::Fiadd
        | Mnemonic::Fsub
        | Mnemonic::Fsubp
        | Mnemonic::Fsubr
        | Mnemonic::Fsubrp
        | Mnemonic::Fisub
        | Mnemonic::Fisubr
        | Mnemonic::Fmul
        | Mnemonic::Fmulp
        | Mnemonic::Fimul
        | Mnemonic::Fdiv
        | Mnemonic::Fdivp
        | Mnemonic::Fdivr
        | Mnemonic::Fdivrp
        | Mnemonic::Fidiv
        | Mnemonic::Fidivr
        | Mnemonic::Fcom
        | Mnemonic::Fcomp
        | Mnemonic::Fcompp
        | Mnemonic::Ficom
        | Mnemonic::Ficomp
        | Mnemonic::Fucom
        | Mnemonic::Fucomp
        | Mnemonic::Fucompp
        | Mnemonic::Fnstsw
        | Mnemonic::Fstsw
        | Mnemonic::Fnstcw
        | Mnemonic::Fstcw
        | Mnemonic::Fldcw
        | Mnemonic::Fchs
        | Mnemonic::Fabs
        | Mnemonic::Fsqrt => {
            if x87::exec_x87(mem, regs, instr)? {
                Ok(())
            } else {
                degrade_fallback(instr, instr.mnemonic())
            }
        }

        other => degrade_fallback(instr, other),
    }
}

/// Degrade-not-die (Wave 4): trace once per mnemonic, count the execution,
/// and continue with partial state (RIP already advanced, no register/memory
/// effects) instead of stopping the session. `WIE_DEGRADE=0` restores the
/// hard stop for bisect.
fn degrade_fallback(instr: &Instruction, other: Mnemonic) -> Result<(), StepExecError> {
    if *DEGRADE_ENABLED {
        DEGRADED_INSNS.fetch_add(1, Ordering::Relaxed);
        let mut seen = degrade_locked_seen();
        if seen.insert(other as u32) {
            tracing::warn!(
                target: "wiecpu",
                ip = format_args!("{:#x}", instr.ip()),
                mnemonic = format!("{other:?}"),
                "unimplemented mnemonic — executing as partial no-op \
                 (degrade-not-die; WIE_DEGRADE=0 restores the stop)"
            );
        }
        Ok(())
    } else {
        Err(StepExecError::Cpu(CpuError::Message(format!(
            "unimplemented mnemonic {other:?} at {:#x}",
            instr.ip()
        ))))
    }
}

fn exec_test(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let a = read_op(mem, regs, instr, 0)?;
    let b = read_op(mem, regs, instr, 1)?;
    let result = (a & b) & regs::size_mask(size);
    regs::set_logic_flags(regs, result, size);
    Ok(())
}

fn exec_inc_dec(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    inc: bool,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let dst = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    let src = 1_u64;
    let result = if inc {
        dst.wrapping_add(src)
    } else {
        dst.wrapping_sub(src)
    };
    let cf = regs.flag(Rflags::CF); // INC/DEC do not modify CF
    if inc {
        regs::set_add_flags(regs, dst, src, result, size);
    } else {
        regs::set_sub_flags(regs, dst, src, result, size);
    }
    regs.set_flag(Rflags::CF, cf);
    write_op(mem, regs, instr, 0, result & regs::size_mask(size))?;
    Ok(())
}

fn exec_neg(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let dst = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    let result = 0_u64.wrapping_sub(dst);
    regs::set_sub_flags(regs, 0, dst, result, size);
    // NEG sets CF if operand was non-zero.
    regs.set_flag(Rflags::CF, dst != 0);
    write_op(mem, regs, instr, 0, result & regs::size_mask(size))?;
    Ok(())
}

fn exec_not(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let dst = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    let result = !dst;
    write_op(mem, regs, instr, 0, result & regs::size_mask(size))?;
    Ok(())
}

/// Lzcnt: count leading zeros of `src` into `dst`.
///
/// A zero `src` yields the operand width (32/64) — the defined difference
/// from Bsr. CF = (src == 0); ZF = (result == 0); other flags undefined
/// (left untouched).
fn exec_lzcnt(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 1)?;
    let bits = regs::size_bits(size);
    let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
    let result = if src == 0 {
        u64::from(bits)
    } else {
        // leading_zeros() counts in 64-bit; a narrower operand masks the top
        // bits to zero, so subtract the 64-bit slack.
        u64::from(src.leading_zeros()).saturating_sub(u64::from(QWORD_BITS.saturating_sub(bits)))
    };
    regs.set_flag(Rflags::CF, src == 0);
    regs.set_flag(Rflags::ZF, result == 0);
    write_op(mem, regs, instr, 0, result)?;
    Ok(())
}

/// Bsr/Bsf: scan `src` for the most significant (`reverse`) / least
/// significant set bit and write its index into `dst`.
///
/// `src == 0` sets ZF and leaves `dst` undefined (written 0); otherwise ZF is
/// cleared. The 64-bit `leading_zeros`/`trailing_zeros` counts are correct for
/// narrower operands because the masked value's upper bits are zero.
fn exec_bit_scan(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    reverse: bool,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 1)?;
    let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
    let result = if src == 0 {
        0
    } else if reverse {
        // Bsr index = 63 - leading_zeros (the 32-bit slack in the 64-bit
        // count cancels, so the formula is width-independent).
        u64::from(SHIFT_MASK_64).saturating_sub(u64::from(src.leading_zeros()))
    } else {
        u64::from(src.trailing_zeros())
    };
    regs.set_flag(Rflags::ZF, src == 0);
    write_op(mem, regs, instr, 0, result)?;
    Ok(())
}

fn exec_shift(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    kind: ShiftKind,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let bits = size.saturating_mul(8);
    let mask = regs::size_mask(size);
    let dst = read_op(mem, regs, instr, 0)? & mask;
    let count_raw = read_op(mem, regs, instr, 1)? as u32;
    // 64-bit operands mask the count with 0x3F; narrower operands with 0x1F.
    let count_masked = count_raw
        & if u32::try_from(bits).unwrap_or(u32::MAX) >= QWORD_BITS {
            SHIFT_MASK_64
        } else {
            SHIFT_MASK_32
        };
    // Rotate-through-carry treats the operand as `width + 1` bits (CF + data);
    // plain shifts/rotates reduce modulo the operand width.
    let width = u32::try_from(bits).unwrap_or(64);
    let rot_width = if matches!(kind, ShiftKind::Rcl | ShiftKind::Rcr) {
        width.saturating_add(1)
    } else {
        width
    };
    let count_mod = if rot_width == 0 {
        0
    } else {
        count_masked % rot_width
    };
    if count_mod == 0 {
        return Ok(());
    }
    let count_usize = count_mod as usize;
    let cf_in = regs.flag(Rflags::CF);
    let (result, cf) = match kind {
        ShiftKind::Shl => {
            let cf_bit = if count_usize <= bits {
                ((dst << (count_usize.saturating_sub(1))) >> bits.saturating_sub(1)) & 1
            } else {
                0
            };
            ((dst << count_mod) & mask, cf_bit != 0)
        }
        ShiftKind::Shr => {
            let cf_bit = (dst >> count_mod.saturating_sub(1)) & 1;
            ((dst >> count_mod) & mask, cf_bit != 0)
        }
        ShiftKind::Sar => {
            let sign_bits = 64_u32.saturating_sub(u32::try_from(bits).unwrap_or(64));
            let signed = ((dst as i64) << sign_bits) >> sign_bits;
            let cf_bit = ((signed as u64) >> count_mod.saturating_sub(1)) & 1;
            let r = ((signed >> count_mod) as u64) & mask;
            (r, cf_bit != 0)
        }
        ShiftKind::Rol => {
            let r = ((dst << count_mod) | (dst >> bits.saturating_sub(count_usize))) & mask;
            let cf_bit = r & 1;
            (r, cf_bit != 0)
        }
        ShiftKind::Ror => {
            let r = ((dst >> count_mod) | (dst << bits.saturating_sub(count_usize))) & mask;
            let cf_bit = (r >> bits.saturating_sub(1)) & 1;
            (r, cf_bit != 0)
        }
        ShiftKind::Rcl => {
            // {CF, dst} as a (width+1)-bit value with CF at bit `width`, rotated left.
            let total = rot_width;
            let t = (u128::from(dst) << 1) | u128::from(cf_in);
            let total_mask = (u128::from(1_u64) << total).wrapping_sub(1);
            let t = ((t << count_mod) | (t >> (total - count_mod))) & total_mask;
            let cf_bit = (t >> u32::try_from(bits).unwrap_or(64)) & 1;
            ((t as u64) & mask, cf_bit != 0)
        }
        ShiftKind::Rcr => {
            // {CF, dst} rotated right.
            let total = rot_width;
            let t = (u128::from(dst) << 1) | u128::from(cf_in);
            let total_mask = (u128::from(1_u64) << total).wrapping_sub(1);
            let t = ((t >> count_mod) | (t << (total - count_mod))) & total_mask;
            let cf_bit = t & 1;
            ((t as u64) & mask, cf_bit != 0)
        }
    };
    regs.set_flag(Rflags::CF, cf);
    // ROL/ROR do not update ZF/SF/PF; SHL/SHR/SAR do.
    if matches!(kind, ShiftKind::Shl | ShiftKind::Shr | ShiftKind::Sar) {
        regs.set_flag(Rflags::ZF, result == 0);
        let sign = regs::size_sign_bit(size);
        regs.set_flag(Rflags::SF, (result & sign) != 0);
        regs.set_flag(Rflags::PF, (result as u8).count_ones().is_multiple_of(2));
    }
    if count_mod == 1 {
        let sign = regs::size_sign_bit(size);
        let of = match kind {
            ShiftKind::Shl => ((result ^ dst) & sign) != 0,
            ShiftKind::Shr => (dst & sign) != 0,
            ShiftKind::Sar => false,
            ShiftKind::Rol => ((result >> bits.saturating_sub(1)) ^ (result & 1)) != 0,
            ShiftKind::Ror => {
                let b1 = (result >> bits.saturating_sub(1)) & 1;
                let b2 = (result >> bits.saturating_sub(2)) & 1;
                b1 != b2
            }
            ShiftKind::Rcl => (cf as u64 ^ ((result >> bits.saturating_sub(1)) & 1)) != 0,
            ShiftKind::Rcr => {
                let b1 = (result >> bits.saturating_sub(1)) & 1;
                let b2 = (result >> bits.saturating_sub(2)) & 1;
                b1 != b2
            }
        };
        regs.set_flag(Rflags::OF, of);
    }
    write_op(mem, regs, instr, 0, result)?;
    Ok(())
}

fn exec_jmp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let target = branch_target(mem, regs, instr)?;
    regs.rip = target;
    Ok(())
}

fn exec_jcc(regs: &mut RegFile, instr: &Instruction, taken: bool) {
    if taken {
        regs.rip = instr.near_branch_target();
    }
}

fn exec_call(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    return_ip: u64,
) -> Result<(), StepExecError> {
    // EA for `call [rsp+…]` must use RSP *before* the return-address push
    // (Intel SDM / Unicorn). Pushing first made us read [rsp+disp-8].
    let target = branch_target(mem, regs, instr)?;
    push_n(mem, regs, return_ip, 8)?;
    regs.rip = target;
    Ok(())
}

fn exec_ret(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let ret = pop_n(mem, regs, 8)?;
    // ret imm16: pop then add imm to RSP
    if instr.op_count() >= 1 && instr.op0_kind() == OpKind::Immediate16 {
        let imm = instr.immediate(0);
        regs.set_rsp(regs.rsp().wrapping_add(imm));
    }
    regs.rip = ret;
    Ok(())
}

fn exec_xchg(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let a = read_op(mem, regs, instr, 0)?;
    let b = read_op(mem, regs, instr, 1)?;
    write_op(mem, regs, instr, 0, b)?;
    write_op(mem, regs, instr, 1, a)?;
    Ok(())
}

/// `XADD r/m, r` — temp = dest; dest = dest + src; src = temp. Flags as ADD.
///
/// With `LOCK` and a memory destination, uses host atomics via soft-translate when
/// the span is aligned and mappable (needed for 7za LZMA2 worker counters).
fn exec_xadd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let mask = regs::size_mask(size);
    let src = read_op(mem, regs, instr, 1)? & mask;

    // Atomic path: LOCK + memory dest (InterlockedIncrement-style RMW).
    if instr.has_lock_prefix() && instr.op0_kind() == OpKind::Memory {
        let addr = effective_address(regs, instr)?;
        if let Some(old) = atomic_fetch_add(mem, addr, size, src) {
            let old_m = old & mask;
            let sum = old_m.wrapping_add(src) & mask;
            // Operand 1 is always a register — write original dest value.
            write_op(mem, regs, instr, 1, old_m)?;
            regs::set_add_flags(regs, old_m, src, sum, size);
            return Ok(());
        }
    }

    let dest = read_op(mem, regs, instr, 0)? & mask;
    let sum = dest.wrapping_add(src) & mask;
    write_op(mem, regs, instr, 0, sum)?;
    write_op(mem, regs, instr, 1, dest)?;
    regs::set_add_flags(regs, dest, src, sum, size);
    Ok(())
}

/// Host-atomic `fetch_add` for guest memory when soft-translate allows.
/// Returns the previous value, or `None` to fall back to non-atomic RMW.
fn atomic_fetch_add(mem: &GuestMemory, addr: u64, size: usize, addend: u64) -> Option<u64> {
    use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
    match size {
        4 if addr.is_multiple_of(4) => {
            let host = mem.host_span(addr, 4, true)?;
            // SAFETY: host_span checked SPC+arena; alignment preserved by soft-translate.
            #[expect(unsafe_code)]
            let atom = unsafe { &*(host.cast::<AtomicI32>()) };
            let add = i32::from_le_bytes((addend as u32).to_le_bytes());
            let old = atom.fetch_add(add, Ordering::SeqCst);
            Some(u64::from(old as u32))
        }
        8 if addr.is_multiple_of(8) => {
            let host = mem.host_span(addr, 8, true)?;
            #[expect(unsafe_code)]
            let atom = unsafe { &*(host.cast::<AtomicI64>()) };
            let add = i64::from_le_bytes(addend.to_le_bytes());
            let old = atom.fetch_add(add, Ordering::SeqCst);
            Some(u64::from_le_bytes(old.to_le_bytes()))
        }
        // 8/16-bit lock xadd: rare; fall back to non-atomic.
        _ => None,
    }
}

/// `CMPXCHG r/m, r` — compare ACC with dest; if equal write src→dest and ZF=1, else dest→ACC and ZF=0.
///
/// Flags follow a CMP of ACC vs dest (same width). `LOCK` is ignored (single-threaded guest).
fn exec_cmpxchg(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let mask = regs::size_mask(size);
    let dest = read_op(mem, regs, instr, 0)? & mask;
    let src = read_op(mem, regs, instr, 1)? & mask;
    let acc = accumulator_value(regs, size)? & mask;

    // Flags as if CMP ACC, dest.
    let result = acc.wrapping_sub(dest);
    regs::set_sub_flags(regs, acc, dest, result, size);

    if regs.flag(Rflags::ZF) {
        write_op(mem, regs, instr, 0, src)?;
    } else {
        write_accumulator(regs, size, dest)?;
    }
    Ok(())
}

fn accumulator_value(regs: &RegFile, size: usize) -> Result<u64, StepExecError> {
    match size {
        1 => Ok(regs.read_reg(Register::AL)?),
        2 => Ok(regs.read_reg(Register::AX)?),
        4 => Ok(regs.read_reg(Register::EAX)?),
        _ => Ok(regs.rax()),
    }
}

fn write_accumulator(regs: &mut RegFile, size: usize, value: u64) -> Result<(), StepExecError> {
    match size {
        1 => Ok(regs.write_reg(Register::AL, value)?),
        2 => Ok(regs.write_reg(Register::AX, value)?),
        4 => Ok(regs.write_reg(Register::EAX, value)?),
        _ => {
            regs.set_rax(value);
            Ok(())
        }
    }
}

/// Shared renderer for mnemonic-keyed diagnostic histograms.
///
/// Scans `counts` (indexed by iced mnemonic discriminant), sorts nonzero
/// entries by count descending, and renders up to 60 rows
/// (`count  tenths-of-a-percent%  name`), an overflow note, and the
/// `--- end ---` footer, bracketed by `make_header(total)`. Returns an empty
/// Vec when every counter is zero and `omit_if_empty` is set (the sampled JIT
/// histogram stays quiet until samples exist; the interpreter dump always
/// prints its bracket lines).
pub(crate) fn render_mnemonic_histogram(
    counts: &[AtomicU64],
    omit_if_empty: bool,
    make_header: impl FnOnce(u64) -> String,
) -> Vec<String> {
    let mut pairs: Vec<(u64, usize)> = Vec::new();
    for (i, c) in counts.iter().enumerate() {
        let count = c.load(Ordering::Relaxed);
        if count > 0 {
            pairs.push((count, i));
        }
    }
    if omit_if_empty && pairs.is_empty() {
        return Vec::new();
    }
    pairs.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total: u64 = pairs.iter().map(|(c, _)| *c).sum();
    let mut lines = vec![make_header(total)];
    let show = pairs.len().min(60);
    for (count, idx) in pairs.iter().take(show) {
        let name = Mnemonic::try_from(*idx)
            .map_or_else(|_| format!("Mnemonic({idx})"), |m| format!("{m:?}"));
        // Integer tenths of a percent (avoid f64 cast_precision_loss).
        let pct = count.saturating_mul(1000).checked_div(total).unwrap_or(0);
        lines.push(format!("{count:>10}  {:3}.{}%  {name}", pct / 10, pct % 10));
    }
    if pairs.len() > show {
        lines.push(format!("  … {} more mnemonics", pairs.len() - show));
    }
    lines.push("--- end ---".to_owned());
    lines
}

/// Dump iced-interpreter mnemonic counters to stderr (sorted by count).
/// Activated by `WIE_EXEC_TRACE=1`. Call after a guest session ends.
pub fn dump_iced_counters() {
    if !*ICED_TRACE_ENABLED {
        return;
    }
    for line in render_mnemonic_histogram(&ICED_COUNTERS, false, |total| {
        format!("--- iced-interp mnemonic counts (total={total}) ---")
    }) {
        tracing::error!("{line}");
    }
}

fn branch_target(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<u64, StepExecError> {
    match instr.op0_kind() {
        OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16 => {
            Ok(instr.near_branch_target())
        }
        OpKind::Register => Ok(regs.read_reg(instr.op_register(0))?),
        OpKind::Memory => read_op(mem, regs, instr, 0),
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "unsupported branch op kind {other:?}"
        )))),
    }
}

fn push_n(
    mem: &GuestMemory,
    regs: &mut RegFile,
    value: u64,
    size: usize,
) -> Result<(), StepExecError> {
    let new_rsp = regs.rsp().wrapping_sub(u64::try_from(size).unwrap_or(8));
    write_mem_value(mem, new_rsp, value, size)?;
    regs.set_rsp(new_rsp);
    Ok(())
}

fn pop_n(mem: &GuestMemory, regs: &mut RegFile, size: usize) -> Result<u64, StepExecError> {
    let rsp = regs.rsp();
    let val = read_mem_value(mem, rsp, size)?;
    regs.set_rsp(rsp.wrapping_add(u64::try_from(size).unwrap_or(8)));
    Ok(val)
}

fn pop64(mem: &GuestMemory, regs: &mut RegFile) -> Result<u64, StepExecError> {
    pop_n(mem, regs, 8)
}

fn read_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
) -> Result<u64, StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register => Ok(regs.read_reg(instr.op_register(op))?),
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            let size = memory_op_size(instr)?;
            read_mem_value(mem, addr, size)
        }
        OpKind::Immediate8
        | OpKind::Immediate8_2nd
        | OpKind::Immediate16
        | OpKind::Immediate32
        | OpKind::Immediate64
        | OpKind::Immediate8to16
        | OpKind::Immediate8to32
        | OpKind::Immediate8to64
        | OpKind::Immediate32to64 => Ok(instr.immediate(op)),
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "unsupported op kind {other:?} for read"
        )))),
    }
}

fn write_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    value: u64,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, op)?;
    write_op_sized(mem, regs, instr, op, value, size)
}

fn write_op_sized(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    value: u64,
    size: usize,
) -> Result<(), StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register => {
            let reg = instr.op_register(op);
            // write_reg applies 8/16 merge and 32-bit zero-extend from reg.size().
            let _ = size;
            regs.write_reg(reg, value)?;
            Ok(())
        }
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            write_mem_value(mem, addr, value, size)
        }
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "unsupported op kind {other:?} for write"
        )))),
    }
}

fn op_size_bytes(instr: &Instruction, op: u32) -> Result<usize, StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register => Ok(instr.op_register(op).size()),
        OpKind::Memory => memory_op_size(instr),
        OpKind::Immediate8 | OpKind::Immediate8_2nd => Ok(1),
        OpKind::Immediate16 | OpKind::Immediate8to16 => Ok(2),
        OpKind::Immediate32 | OpKind::Immediate8to32 => Ok(4),
        OpKind::Immediate64 | OpKind::Immediate8to64 | OpKind::Immediate32to64 => Ok(8),
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "cannot size op kind {other:?}"
        )))),
    }
}

fn memory_op_size(instr: &Instruction) -> Result<usize, StepExecError> {
    let sz = match instr.memory_size() {
        MemorySize::UInt8 | MemorySize::Int8 => 1,
        MemorySize::UInt16 | MemorySize::Int16 => 2,
        MemorySize::UInt32 | MemorySize::Int32 => 4,
        MemorySize::UInt64 | MemorySize::Int64 | MemorySize::QwordOffset | MemorySize::SegPtr64 => {
            8
        }
        other => {
            // Fallback: use size of the other operand if register.
            if instr.op_count() > 0 && instr.op0_kind() == OpKind::Register {
                return Ok(instr.op_register(0).size());
            }
            if instr.op_count() > 1 && instr.op1_kind() == OpKind::Register {
                return Ok(instr.op_register(1).size());
            }
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "unsupported memory size {other:?}"
            ))));
        }
    };
    Ok(sz)
}

pub(crate) fn effective_address(regs: &RegFile, instr: &Instruction) -> Result<u64, StepExecError> {
    // iced stores the absolute address for RIP/EIP-relative in memory_displacement64().
    // For other bases, displacement is a signed offset added to base+index*scale.
    let base = instr.memory_base();
    if base == Register::RIP || base == Register::EIP {
        return Ok(instr.memory_displacement64());
    }

    let mut addr = instr.memory_displacement64();

    // FS/GS segment overrides (x64: FS and GS are the only meaningful segments).
    // Windows x64 uses GS:0 as the per-thread TEB base, which lives at
    // `regs.gs_base()` — the engine's bound TEB page, NOT a process-wide
    // constant. When an instruction carries a GS segment prefix, the effective
    // address is relative to that base, not to address zero.
    let seg = instr.memory_segment();
    if seg == Register::GS || seg == Register::FS {
        addr = addr.wrapping_add(regs.gs_base());
    }
    // Non-IP-relative: treat displacement as signed when displ size is set.
    // iced keeps mem_displ as unsigned bits of the signed field; for pure disp
    // with base/index, virtual_address adds the raw mem_displ then masks.
    // Mirror iced's virtual_address path for 64-bit addressing:
    if base != Register::None {
        addr = addr.wrapping_add(regs.read_reg(base)?);
    }
    let index = instr.memory_index();
    if index != Register::None {
        let scale = u64::from(instr.memory_index_scale());
        let idx_val = regs.read_reg(index)?;
        addr = addr.wrapping_add(idx_val.wrapping_mul(scale));
    }
    Ok(addr)
}

pub(crate) fn read_mem_value(
    mem: &GuestMemory,
    addr: u64,
    size: usize,
) -> Result<u64, StepExecError> {
    if size == 0 || size > 8 {
        return Err(StepExecError::Cpu(CpuError::Message(format!(
            "bad mem read size {size}"
        ))));
    }
    let mut buf = [0_u8; 8];
    let slice = buf
        .get_mut(..size)
        .ok_or_else(|| StepExecError::Cpu(CpuError::Message("mem read buffer".into())))?;
    if let Err(e) = mem.read(addr, slice) {
        drop(e);
        return Err(StepExecError::InvalidMemory(InvalidMem {
            access_type: AccessType::Read,
            address: addr,
            size: i32::try_from(size).unwrap_or(0),
            value: 0,
        }));
    }
    Ok(match size {
        1 => u64::from(buf[0]),
        2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])),
        4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
        8 => u64::from_le_bytes(buf),
        _ => 0,
    })
}

pub(crate) fn write_mem_value(
    mem: &GuestMemory,
    addr: u64,
    value: u64,
    size: usize,
) -> Result<(), StepExecError> {
    if size == 0 || size > 8 {
        return Err(StepExecError::Cpu(CpuError::Message(format!(
            "bad mem write size {size}"
        ))));
    }
    let bytes = value.to_le_bytes();
    let slice = bytes
        .get(..size)
        .ok_or_else(|| StepExecError::Cpu(CpuError::Message("mem write buffer".into())))?;
    if let Err(e) = mem.write(addr, slice) {
        drop(e);
        return Err(StepExecError::InvalidMemory(InvalidMem {
            access_type: AccessType::Write,
            address: addr,
            size: i32::try_from(size).unwrap_or(0),
            value: i64::try_from(value).unwrap_or(0),
        }));
    }
    Ok(())
}
