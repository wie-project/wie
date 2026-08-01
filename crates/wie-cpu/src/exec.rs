//! Instruction execution for the iced x86-64 interpreter.
//!
//! Low-level CPU arithmetic intentionally uses wrapping ops, truncating casts,
//! and direct indexing of fixed-size buffers — clippy pedantic is not useful here.

#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss, // cvtsi2ss/sd: Intel-defined rounding, not a bug
    clippy::cast_sign_loss,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::integer_division,
    clippy::many_single_char_names, // lane helpers (a/b/x/y/mask)
    clippy::float_cmp, // COMISS equality: IEEE == is the architectural result
    clippy::manual_range_contains // f >= hi || f < lo reads clearer than !range
)]

use crate::CpuError;
use crate::mem::GuestMemory;
use crate::regs::{self, RegFile, rflags};
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Access type codes matching Unicorn-ish invalid-memory reporting (0=read,1=write,2=fetch).
pub(crate) const ACCESS_READ: i32 = 0;
pub(crate) const ACCESS_WRITE: i32 = 1;
pub(crate) const ACCESS_FETCH: i32 = 16;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvalidMem {
    pub access_type: i32,
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

/// Iced-interpreter mnemonic counters (activated by `WIE_EXEC_TRACE=1`).
/// Works in release too — 7za residual discovery should not require a debug build.
static ICED_COUNTERS: LazyLock<Box<[AtomicU64]>> = LazyLock::new(|| {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(2048)
        .collect()
});
static ICED_TRACE_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WIE_EXEC_TRACE").is_ok_and(|v| v == "1"));

/// Direct-mapped RIP → decoded `Instruction` cache (per thread).
///
/// Every iced step used to do a full `Decoder::with_ip` + `decode()` pair — the
/// dominant cost on interpreter-bound loops. This cache stores the last
/// `DECODE_CACHE_SLOTS` distinct instruction decodes indexed by `rip`, tagged
/// with the `GuestMemory::generation` snapshot so any protect / free / SMC
/// invalidation naturally shoots the whole cache without explicit clearing.
///
/// `Instruction` is `Copy` (~32 bytes); at 512 slots this is ~24 KiB per thread.
const DECODE_CACHE_SLOTS: usize = 512;

#[derive(Clone, Copy)]
struct DecodeSlot {
    /// RIP tag; `u64::MAX` when the slot is cold.
    rip: u64,
    /// `GuestMemory::generation` when the decode was captured.
    mem_gen: u64,
    /// Cached iced-x86 `Instruction`.
    instr: Instruction,
    /// Instruction length in bytes (0 when cold).
    len: u32,
}

impl DecodeSlot {
    fn empty() -> Self {
        // `Instruction::new` is not const in this iced-x86 version, so this
        // helper stays a plain fn (called at thread-local init time only).
        Self {
            rip: u64::MAX,
            mem_gen: 0,
            instr: Instruction::new(),
            len: 0,
        }
    }
}

thread_local! {
    static DECODE_CACHE: std::cell::RefCell<Box<[DecodeSlot; DECODE_CACHE_SLOTS]>> =
        std::cell::RefCell::new({
            // Build a Vec then reify to a fixed-size Box<[T; N]>. Vec<T>::into_boxed_slice
            // returns Box<[T]>; TryFrom<Box<[T]>> for Box<[T; N]> handles the resize.
            let vec: Vec<DecodeSlot> = (0..DECODE_CACHE_SLOTS).map(|_| DecodeSlot::empty()).collect();
            vec.into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| Box::new([DecodeSlot::empty(); DECODE_CACHE_SLOTS]))
        });
}

#[inline]
fn decode_slot_index(rip: u64) -> usize {
    // Fold in high and mid bits so nearby RIPs (single-instruction advance)
    // spread across the cache instead of colliding on the same slot.
    let x = rip ^ (rip >> 12) ^ (rip >> 28);
    (x as usize) & (DECODE_CACHE_SLOTS - 1)
}

/// Look up (or fill) a cached iced decode at `rip`. Returns `(instruction, length)`.
///
/// A cache miss re-runs the iced-x86 decoder. Hits under the same guest-memory
/// generation return the cached instruction without touching the decoder.
fn decode_at(mem: &GuestMemory, rip: u64) -> Option<(Instruction, u32)> {
    let gen_now = mem.generation();
    let idx = decode_slot_index(rip);
    let cached = DECODE_CACHE.with(|c| c.borrow().get(idx).copied());
    if let Some(slot) = cached
        && slot.rip == rip
        && slot.mem_gen == gen_now
        && slot.len != 0
    {
        return Some((slot.instr, slot.len));
    }
    // Miss: fetch + decode + fill.
    let mut fetch_buf = [0_u8; 15];
    let n = mem.fetch_into(rip, &mut fetch_buf).ok()?;
    let mut decoder =
        iced_x86::Decoder::with_ip(64, fetch_buf.get(..n)?, rip, iced_x86::DecoderOptions::NONE);
    let instr = decoder.decode();
    if instr.is_invalid() || instr.len() == 0 {
        return None;
    }
    let len = u32::try_from(instr.len()).ok()?;
    DECODE_CACHE.with(|c| {
        if let Some(dst) = c.borrow_mut().get_mut(idx) {
            *dst = DecodeSlot {
                rip,
                mem_gen: gen_now,
                instr,
                len,
            };
        }
    });
    Some((instr, len))
}

/// Invalidate the entire per-thread iced decode cache.
///
/// Callers use this when they know something perturbed guest code without
/// bumping [`GuestMemory::generation`] (e.g. an explicit `FlushInstructionCache`
/// or a SMC path that wants to be safe). Exposed but unused today; kept for
/// future SMC/`FlushInstructionCache` hookup.
#[allow(dead_code)]
pub(crate) fn iced_decode_cache_flush() {
    DECODE_CACHE.with(|c| {
        for slot in c.borrow_mut().iter_mut() {
            *slot = DecodeSlot::empty();
        }
    });
}

/// Decode + execute one instruction at `regs.rip`.
///
/// Lightweight tracer: the first time each non-JIT mnemonic is interpreted,
/// it is logged once at `info` level (opt-in: `WIE_EXEC_TRACE=1`).  Run with
/// `WIE_EXEC_TRACE=1 7za …` to discover which instructions keep code in the
/// interpreter rather than the JIT.
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
                access_type: ACCESS_FETCH,
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

#[allow(dead_code)] // retained for external callers that only want a length
fn peek_insn_len(mem: &GuestMemory, rip: u64) -> Option<u32> {
    decode_at(mem, rip).map(|(_, len)| len)
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
        | Mnemonic::Pause => Ok(()),

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

        Mnemonic::Shl | Mnemonic::Sal => exec_shift(mem, regs, instr, ShiftKind::Shl),
        Mnemonic::Shr => exec_shift(mem, regs, instr, ShiftKind::Shr),
        Mnemonic::Sar => exec_shift(mem, regs, instr, ShiftKind::Sar),
        Mnemonic::Rol => exec_shift(mem, regs, instr, ShiftKind::Rol),
        Mnemonic::Ror => exec_shift(mem, regs, instr, ShiftKind::Ror),

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
            exec_jcc(regs, instr, cond_from_jcc(m, regs));
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
        | Mnemonic::Cmovnp) => exec_cmov(mem, regs, instr, cond_from_cmov(m, regs)),

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
        | Mnemonic::Setnp) => exec_setcc(mem, regs, instr, cond_from_setcc(m, regs)),

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
            regs.set_flag(rflags::DF, false);
            Ok(())
        }
        Mnemonic::Std => {
            regs.set_flag(rflags::DF, true);
            Ok(())
        }
        Mnemonic::Clc => {
            regs.set_flag(rflags::CF, false);
            Ok(())
        }
        Mnemonic::Stc => {
            regs.set_flag(rflags::CF, true);
            Ok(())
        }
        Mnemonic::Cmc => {
            regs.set_flag(rflags::CF, !regs.flag(rflags::CF));
            Ok(())
        }
        Mnemonic::Pushfq => {
            push_n(mem, regs, regs.rflags, 8)?;
            Ok(())
        }
        Mnemonic::Popfq => {
            let v = pop_n(mem, regs, 8)?;
            // Keep reserved bit 1 set.
            regs.rflags = (v & !rflags::ALWAYS1) | rflags::ALWAYS1;
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
        Mnemonic::Cmpsd => exec_cmps(mem, regs, instr, 4),
        Mnemonic::Cmpsq => exec_cmps(mem, regs, instr, 8),

        // Scalar / packed SSE moves (enough for CRT / memcpy helpers).
        Mnemonic::Movss => exec_sse_mov(mem, regs, instr, 4, true),
        Mnemonic::Movaps
        | Mnemonic::Movups
        | Mnemonic::Movdqa
        | Mnemonic::Movdqu
        | Mnemonic::Movapd
        | Mnemonic::Movupd => exec_sse_mov(mem, regs, instr, 16, false),
        Mnemonic::Movq => exec_sse_movq(mem, regs, instr),
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
        Mnemonic::Movhps => exec_sse_movhps(mem, regs, instr),
        Mnemonic::Movhlps | Mnemonic::Movlhps => exec_sse_movhlps(regs, instr),
        // SSE2 unpack / shuffle / compare.
        Mnemonic::Punpcklqdq | Mnemonic::Punpckhqdq => exec_sse_punpck(regs, instr),
        Mnemonic::Punpcklbw
        | Mnemonic::Punpcklwd
        | Mnemonic::Punpckldq
        | Mnemonic::Punpckhbw
        | Mnemonic::Punpckhwd
        | Mnemonic::Punpckhdq => exec_sse_punpck_lanes(mem, regs, instr),
        Mnemonic::Pshufd => exec_sse_pshufd(mem, regs, instr),
        Mnemonic::Pshuflw | Mnemonic::Pshufhw => exec_sse_pshuflw_hw(mem, regs, instr),
        Mnemonic::Pshufb => exec_sse_pshufb(mem, regs, instr),
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
        Mnemonic::Psadbw => exec_sse_psadbw(mem, regs, instr),
        Mnemonic::Unpcklpd => exec_sse_unpcklpd(regs, instr),
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
        // Integer ↔ FP converts.
        Mnemonic::Cvtsi2ss | Mnemonic::Cvtsi2sd => exec_sse_cvt_gpr_to_fp(mem, regs, instr),
        Mnemonic::Cvttss2si
        | Mnemonic::Cvtss2si
        | Mnemonic::Cvttsd2si
        | Mnemonic::Cvtsd2si => exec_sse_cvt_fp_to_gpr(mem, regs, instr),
        Mnemonic::Cvtps2dq | Mnemonic::Cvtdq2ps | Mnemonic::Cvttps2dq => {
            exec_sse_cvt_packed(mem, regs, instr)
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

        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "unimplemented mnemonic {other:?} at {:#x}",
            instr.ip()
        )))),
    }
}

#[derive(Clone, Copy)]
enum BitOp {
    Bt,
    Bts,
    Btr,
    Btc,
}

#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Adc,
    Sub,
    Sbb,
    Xor,
    Or,
    And,
    Cmp,
}

#[derive(Clone, Copy)]
enum ShiftKind {
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
}

fn cond_from_jcc(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Je => regs.flag(rflags::ZF),
        Mnemonic::Jne => !regs.flag(rflags::ZF),
        Mnemonic::Ja => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Jae => !regs.flag(rflags::CF),
        Mnemonic::Jb => regs.flag(rflags::CF),
        Mnemonic::Jbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Jg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Jge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Jl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Jle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Jo => regs.flag(rflags::OF),
        Mnemonic::Jno => !regs.flag(rflags::OF),
        Mnemonic::Js => regs.flag(rflags::SF),
        Mnemonic::Jns => !regs.flag(rflags::SF),
        Mnemonic::Jp => regs.flag(rflags::PF),
        Mnemonic::Jnp => !regs.flag(rflags::PF),
        _ => false,
    }
}

fn cond_from_cmov(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Cmove => regs.flag(rflags::ZF),
        Mnemonic::Cmovne => !regs.flag(rflags::ZF),
        Mnemonic::Cmova => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Cmovae => !regs.flag(rflags::CF),
        Mnemonic::Cmovb => regs.flag(rflags::CF),
        Mnemonic::Cmovbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Cmovg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Cmovge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Cmovl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Cmovle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Cmovo => regs.flag(rflags::OF),
        Mnemonic::Cmovno => !regs.flag(rflags::OF),
        Mnemonic::Cmovs => regs.flag(rflags::SF),
        Mnemonic::Cmovns => !regs.flag(rflags::SF),
        Mnemonic::Cmovp => regs.flag(rflags::PF),
        Mnemonic::Cmovnp => !regs.flag(rflags::PF),
        _ => false,
    }
}

fn cond_from_setcc(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Sete => regs.flag(rflags::ZF),
        Mnemonic::Setne => !regs.flag(rflags::ZF),
        Mnemonic::Seta => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Setae => !regs.flag(rflags::CF),
        Mnemonic::Setb => regs.flag(rflags::CF),
        Mnemonic::Setbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Setg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Setge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Setl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Setle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Seto => regs.flag(rflags::OF),
        Mnemonic::Setno => !regs.flag(rflags::OF),
        Mnemonic::Sets => regs.flag(rflags::SF),
        Mnemonic::Setns => !regs.flag(rflags::SF),
        Mnemonic::Setp => regs.flag(rflags::PF),
        Mnemonic::Setnp => !regs.flag(rflags::PF),
        _ => false,
    }
}

fn exec_mov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_op(mem, regs, instr, 1)?;
    write_op(mem, regs, instr, 0, src)?;
    Ok(())
}

fn exec_movzx(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    sign: bool,
) -> Result<(), StepExecError> {
    let src_size = op_size_bytes(instr, 1)?;
    let dst_size = op_size_bytes(instr, 0)?;
    let raw = read_op(mem, regs, instr, 1)?;
    let src_mask = regs::size_mask(src_size);
    let narrow = raw & src_mask;
    let extended = if sign {
        let bits = src_size.saturating_mul(8);
        let shift = 64_u32.saturating_sub(u32::try_from(bits).unwrap_or(64));
        ((narrow as i64) << shift >> shift) as u64
    } else {
        narrow
    };
    // Write with dst size semantics (32-bit zero-extends).
    write_op_sized(mem, regs, instr, 0, extended, dst_size)?;
    Ok(())
}

fn exec_lea(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let addr = effective_address(regs, instr)?;
    let dst = instr.op_register(0);
    // LEA writes full register size of dest.
    regs.write_reg(dst, addr)?;
    Ok(())
}

fn exec_push(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let val = read_op(mem, regs, instr, 0)?;
    // In 64-bit mode push is always 64-bit (except rare 16-bit override).
    let size = match instr.op0_kind() {
        OpKind::Register if instr.op_register(0).size() == 2 => 2_usize,
        _ => 8_usize,
    };
    push_n(mem, regs, val, size)
}

fn exec_pop(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = match instr.op0_kind() {
        OpKind::Register if instr.op_register(0).size() == 2 => 2_usize,
        _ => 8_usize,
    };
    let val = pop_n(mem, regs, size)?;
    write_op(mem, regs, instr, 0, val)
}

fn exec_arith(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: ArithOp,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let dst = read_op(mem, regs, instr, 0)?;
    let src = read_op(mem, regs, instr, 1)?;
    let mask = regs::size_mask(size);
    let d = dst & mask;
    let s = src & mask;
    let cf = u64::from(regs.flag(rflags::CF));
    let result = match op {
        ArithOp::Add => d.wrapping_add(s),
        ArithOp::Adc => d.wrapping_add(s).wrapping_add(cf),
        ArithOp::Sub | ArithOp::Cmp => d.wrapping_sub(s),
        ArithOp::Sbb => d.wrapping_sub(s).wrapping_sub(cf),
        ArithOp::Xor => d ^ s,
        ArithOp::Or => d | s,
        ArithOp::And => d & s,
    };
    match op {
        ArithOp::Add => {
            regs::set_add_flags(regs, d, s, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Adc => {
            // Flags from full add with carry-in.
            let wide = u128::from(d)
                .wrapping_add(u128::from(s))
                .wrapping_add(u128::from(cf));
            regs::set_add_flags(regs, d, s.wrapping_add(cf), result, size);
            regs.set_flag(rflags::CF, wide > u128::from(mask));
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Sub => {
            regs::set_sub_flags(regs, d, s, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Sbb => {
            // CF/OF must use full-width borrow: when `s + CF` overflows the
            // operand, masking `borrow` to `size` zeros it and wrongly clears CF
            // (breaks MSVC `cmp; sbb r,r; sbb r,-1` equality idioms used in 7-Zip QI).
            let wide_src = u128::from(s).wrapping_add(u128::from(cf));
            let r = result & mask;
            regs::set_sub_flags(regs, d, s, result, size);
            regs.set_flag(rflags::CF, u128::from(d) < wide_src);
            let d_s = i128::from(sign_extend(d, size));
            let s_s = i128::from(sign_extend(s, size));
            let expected = d_s.wrapping_sub(s_s).wrapping_sub(i128::from(cf != 0));
            let got = i128::from(sign_extend(r, size));
            regs.set_flag(rflags::OF, expected != got);
            write_op(mem, regs, instr, 0, r)?;
        }
        ArithOp::Cmp => {
            regs::set_sub_flags(regs, d, s, result, size);
        }
        ArithOp::Xor | ArithOp::Or | ArithOp::And => {
            regs::set_logic_flags(regs, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
    }
    Ok(())
}

fn exec_imul(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    // Forms: 1-op (RAX/RDX), 2-op (dst *= src), 3-op (dst = src1 * imm).
    let nops = instr.op_count();
    match nops {
        1 => {
            let size = op_size_bytes(instr, 0)?;
            let src = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let a = regs.rax() & regs::size_mask(size);
            let product = i128::from(sign_extend(a, size)) * i128::from(sign_extend(src, size));
            write_imul_product(regs, product, size)?;
            Ok(())
        }
        2 => {
            let size = op_size_bytes(instr, 0)?;
            let dst = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
            let product = i128::from(sign_extend(dst, size)) * i128::from(sign_extend(src, size));
            let lo = (product as u64) & regs::size_mask(size);
            write_op(mem, regs, instr, 0, lo)?;
            set_imul_flags(regs, product, size);
            Ok(())
        }
        3 => {
            let size = op_size_bytes(instr, 0)?;
            let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
            let imm = read_op(mem, regs, instr, 2)? & regs::size_mask(size);
            let product = i128::from(sign_extend(src, size)) * i128::from(sign_extend(imm, size));
            let lo = (product as u64) & regs::size_mask(size);
            write_op(mem, regs, instr, 0, lo)?;
            set_imul_flags(regs, product, size);
            Ok(())
        }
        _ => Err(StepExecError::Cpu(CpuError::Message(format!(
            "imul with {nops} operands"
        )))),
    }
}

fn exec_mul(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let src = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    let a = regs.rax() & regs::size_mask(size);
    let product = u128::from(a).wrapping_mul(u128::from(src));
    match size {
        1 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            let hi = (product >> 8) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        2 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            regs.write_reg(Register::DX, ((product >> 16) as u64) & 0xffff)?;
            let hi = (product >> 16) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        4 => {
            regs.write_reg(Register::EAX, product as u64 & 0xffff_ffff)?;
            regs.write_reg(Register::EDX, ((product >> 32) as u64) & 0xffff_ffff)?;
            let hi = (product >> 32) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        _ => {
            regs.set_rax(product as u64);
            regs.set_rdx((product >> 64) as u64);
            let hi = (product >> 64) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
    }
    Ok(())
}

fn write_imul_product(regs: &mut RegFile, product: i128, size: usize) -> Result<(), StepExecError> {
    match size {
        1 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
        }
        2 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            regs.write_reg(Register::DX, ((product >> 16) as u64) & 0xffff)?;
        }
        4 => {
            regs.write_reg(Register::EAX, product as u64 & 0xffff_ffff)?;
            regs.write_reg(Register::EDX, ((product >> 32) as u64) & 0xffff_ffff)?;
        }
        _ => {
            regs.set_rax(product as u64);
            regs.set_rdx((product >> 64) as u64);
        }
    }
    set_imul_flags(regs, product, size);
    Ok(())
}

fn set_imul_flags(regs: &mut RegFile, product: i128, size: usize) {
    // CF/OF set if high half is not sign-extension of low half.
    let bits = size.saturating_mul(8);
    let lo_bits = bits.min(64);
    let lo = product as u64
        & if lo_bits >= 64 {
            u64::MAX
        } else {
            (1_u64 << lo_bits).wrapping_sub(1)
        };
    let sign_ext = if (lo >> (lo_bits.saturating_sub(1))) & 1 == 1 {
        // negative: high should be all ones for width
        match size {
            1 => i128::from(lo as i8),
            2 => i128::from(lo as i16),
            4 => i128::from(lo as i32),
            _ => i128::from(lo as i64),
        }
    } else {
        i128::from(lo)
    };
    // For 1-op IMUL the full product width is 2*size; for 2/3-op only low size is stored.
    // CF/OF = product does not fit in size bytes as signed.
    let max = match size {
        1 => i128::from(i8::MAX),
        2 => i128::from(i16::MAX),
        4 => i128::from(i32::MAX),
        _ => i128::from(i64::MAX),
    };
    let min = match size {
        1 => i128::from(i8::MIN),
        2 => i128::from(i16::MIN),
        4 => i128::from(i32::MIN),
        _ => i128::from(i64::MIN),
    };
    let overflow = product < min || product > max;
    let _ = sign_ext;
    regs.set_flag(rflags::CF, overflow);
    regs.set_flag(rflags::OF, overflow);
}

fn sign_extend(value: u64, size: usize) -> i64 {
    let bits = size.saturating_mul(8).min(64);
    let shift = 64_u32.saturating_sub(u32::try_from(bits).unwrap_or(64));
    ((value as i64) << shift) >> shift
}

fn exec_div(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    signed: bool,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let divisor = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    if divisor == 0 {
        return Err(StepExecError::Cpu(CpuError::DivideByZero(instr.ip())));
    }

    match size {
        1 => {
            let dividend = regs.read_reg(Register::AX)? & 0xffff;
            if signed {
                let num = dividend as i16;
                let den = sign_extend(divisor, 1) as i16;
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                let ax = u64::from(u16::from(r as u8) << 8 | u16::from(q as u8));
                regs.write_reg(Register::AX, ax)?;
            } else {
                let q = dividend / divisor;
                let r = dividend % divisor;
                if q > 0xff {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::AX, (r & 0xff) << 8 | (q & 0xff))?;
            }
        }
        2 => {
            let lo = regs.read_reg(Register::AX)? & 0xffff;
            let hi = regs.read_reg(Register::DX)? & 0xffff;
            if signed {
                // DX:AX as i32
                let num = (i32::from(hi as i16) << 16) | i32::from(lo as u16);
                let den = sign_extend(divisor, 2) as i32;
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if !(-32768..=32767).contains(&q) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.write_reg(Register::AX, q as u64 & 0xffff)?;
                regs.write_reg(Register::DX, r as u64 & 0xffff)?;
            } else {
                let dividend = (hi << 16) | lo;
                let q = dividend / divisor;
                let r = dividend % divisor;
                if q > 0xffff {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::AX, q & 0xffff)?;
                regs.write_reg(Register::DX, r & 0xffff)?;
            }
        }
        4 => {
            let lo = regs.read_reg(Register::EAX)? & 0xffff_ffff;
            let hi = regs.read_reg(Register::EDX)? & 0xffff_ffff;
            if signed {
                let num = (i64::from(hi as i32) << 32) | i64::from(lo as u32);
                let den = sign_extend(divisor, 4);
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&q) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.write_reg(Register::EAX, q as u64 & 0xffff_ffff)?;
                regs.write_reg(Register::EDX, r as u64 & 0xffff_ffff)?;
            } else {
                let dividend = (u128::from(hi) << 32) | u128::from(lo);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                if q > u128::from(u32::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::EAX, q as u64 & 0xffff_ffff)?;
                regs.write_reg(Register::EDX, r as u64 & 0xffff_ffff)?;
            }
        }
        _ => {
            let lo = regs.rax();
            let hi = regs.rdx();
            if signed {
                let num = (i128::from(hi as i64) << 64) | i128::from(lo);
                let den = i128::from(sign_extend(divisor, 8));
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if q < i128::from(i64::MIN) || q > i128::from(i64::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.set_rax(q as u64);
                regs.set_rdx(r as u64);
            } else {
                let dividend = (u128::from(hi) << 64) | u128::from(lo);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                if q > u128::from(u64::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.set_rax(q as u64);
                regs.set_rdx(r as u64);
            }
        }
    }
    Ok(())
}

fn exec_cmov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    taken: bool,
) -> Result<(), StepExecError> {
    if taken {
        let src = read_op(mem, regs, instr, 1)?;
        write_op(mem, regs, instr, 0, src)?;
    }
    Ok(())
}

fn exec_setcc(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    taken: bool,
) -> Result<(), StepExecError> {
    write_op(mem, regs, instr, 0, u64::from(taken))?;
    Ok(())
}

fn exec_bswap(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let reg = instr.op_register(0);
    let v = regs.read_reg(reg)?;
    let size = reg.size();
    let swapped = match size {
        4 => u64::from((v as u32).swap_bytes()),
        8 => v.swap_bytes(),
        _ => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "bswap size {size}"
            ))));
        }
    };
    regs.write_reg(reg, swapped)?;
    Ok(())
}

fn exec_bit(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: BitOp,
) -> Result<(), StepExecError> {
    let bit_offset = read_op(mem, regs, instr, 1)?;
    match instr.op0_kind() {
        OpKind::Register => {
            let size = instr.op_register(0).size();
            let bits = size.saturating_mul(8);
            let idx = (bit_offset as u32) % u32::try_from(bits).unwrap_or(64);
            let val = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let mask = 1_u64 << idx;
            let cf = (val & mask) != 0;
            regs.set_flag(rflags::CF, cf);
            let new = match op {
                BitOp::Bt => val,
                BitOp::Bts => val | mask,
                BitOp::Btr => val & !mask,
                BitOp::Btc => val ^ mask,
            };
            if !matches!(op, BitOp::Bt) {
                write_op(mem, regs, instr, 0, new)?;
            }
        }
        OpKind::Memory => {
            // Memory bit string: EA + signed(offset)/8, bit = offset & 7.
            let base = effective_address(regs, instr)?;
            let off = bit_offset as i64;
            let byte_delta = off.div_euclid(8);
            let bit = u32::try_from(off.rem_euclid(8)).unwrap_or(0);
            let addr = base.wrapping_add(byte_delta as u64);
            let mut b = [0_u8; 1];
            match mem.read(addr, &mut b) {
                Ok(()) => {}
                Err(e) => {
                    drop(e);
                    return Err(StepExecError::InvalidMemory(InvalidMem {
                        access_type: ACCESS_READ,
                        address: addr,
                        size: 1,
                        value: 0,
                    }));
                }
            }
            let val = u64::from(b[0]);
            let mask = 1_u64 << bit;
            let cf = (val & mask) != 0;
            regs.set_flag(rflags::CF, cf);
            if !matches!(op, BitOp::Bt) {
                let new = match op {
                    BitOp::Bt => val,
                    BitOp::Bts => val | mask,
                    BitOp::Btr => val & !mask,
                    BitOp::Btc => val ^ mask,
                };
                write_mem_value(mem, addr, new, 1)?;
            }
        }
        other => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "bt op0 kind {other:?}"
            ))));
        }
    }
    Ok(())
}

fn is_sse_movsd(instr: &Instruction) -> bool {
    instr.op0_register().is_xmm()
        || instr.op1_register().is_xmm()
        || matches!(
            instr.code(),
            iced_x86::Code::Movsd_xmm_xmmm64 | iced_x86::Code::Movsd_xmmm64_xmm
        )
}

fn exec_sse_mov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    nbytes: usize,
    scalar_merge: bool,
) -> Result<(), StepExecError> {
    let val = read_sse_op(mem, regs, instr, 1, nbytes)?;
    write_sse_op(mem, regs, instr, 0, val, nbytes, scalar_merge)
}

#[derive(Clone, Copy)]
enum SseBitOp {
    Xor,
    And,
    Or,
    Andn,
}

fn exec_sse_bitwise(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseBitOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let r = match op {
        SseBitOp::Xor => a ^ b,
        SseBitOp::And => a & b,
        SseBitOp::Or => a | b,
        SseBitOp::Andn => (!a) & b,
    };
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

#[derive(Clone, Copy)]
enum FpOp {
    Add,
    Sub,
    Mul,
    Div,
}

fn fp32(op: FpOp, a: f32, b: f32) -> f32 {
    match op {
        FpOp::Add => a + b,
        FpOp::Sub => a - b,
        FpOp::Mul => a * b,
        FpOp::Div => a / b,
    }
}

fn fp64(op: FpOp, a: f64, b: f64) -> f64 {
    match op {
        FpOp::Add => a + b,
        FpOp::Sub => a - b,
        FpOp::Mul => a * b,
        FpOp::Div => a / b,
    }
}

fn exec_sse_scalar_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: FpOp,
    is_f64: bool,
) -> Result<(), StepExecError> {
    if is_f64 {
        let a = read_sse_op(mem, regs, instr, 0, 8)?;
        let b = read_sse_op(mem, regs, instr, 1, 8)?;
        let fa = f64::from_bits(a as u64);
        let fb = f64::from_bits(b as u64);
        let r = u128::from(fp64(op, fa, fb).to_bits());
        write_sse_op(mem, regs, instr, 0, r, 8, true)
    } else {
        let a = read_sse_op(mem, regs, instr, 0, 4)?;
        let b = read_sse_op(mem, regs, instr, 1, 4)?;
        let fa = f32::from_bits(a as u32);
        let fb = f32::from_bits(b as u32);
        let r = u128::from(fp32(op, fa, fb).to_bits());
        write_sse_op(mem, regs, instr, 0, r, 4, true)
    }
}

fn exec_sse_packed_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: FpOp,
    is_f64: bool,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let mut out = 0_u128;
    if is_f64 {
        for i in 0..2 {
            let shift = i * 64;
            let fa = f64::from_bits(((a >> shift) & u128::from(u64::MAX)) as u64);
            let fb = f64::from_bits(((b >> shift) & u128::from(u64::MAX)) as u64);
            out |= u128::from(fp64(op, fa, fb).to_bits()) << shift;
        }
    } else {
        for i in 0..4 {
            let shift = i * 32;
            let fa = f32::from_bits(((a >> shift) & 0xffff_ffff) as u32);
            let fb = f32::from_bits(((b >> shift) & 0xffff_ffff) as u32);
            out |= u128::from(fp32(op, fa, fb).to_bits()) << shift;
        }
    }
    write_sse_op(mem, regs, instr, 0, out, 16, false)
}

fn exec_sse_movq(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    // `MOVQ xmm, r/m64`: low quadword moved, bits [127:64] cleared.
    if instr.op0_register().is_xmm() {
        let v = read_sse_op(mem, regs, instr, 1, 8)?;
        let v = v & u128::from(u64::MAX);
        regs.write_xmm(instr.op_register(0), v)?;
        return Ok(());
    }
    // `MOVQ r/m64, xmm`: extract the low quadword.
    if instr.op1_register().is_xmm() {
        let v = regs.read_xmm(instr.op_register(1))? as u64;
        if instr.op0_kind() == OpKind::Memory {
            write_mem_value(mem, effective_address(regs, instr)?, v, 8)?;
        } else {
            regs.write_reg(instr.op_register(0), v)?;
        }
        return Ok(());
    }
    Err(StepExecError::Cpu(CpuError::Message(
        "movq without xmm".into(),
    )))
}

fn exec_sse_movd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    if instr.op0_register().is_xmm() {
        let v = if instr.op1_kind() == OpKind::Memory {
            read_mem_value(mem, effective_address(regs, instr)?, 4)?
        } else {
            regs.read_reg(instr.op_register(1))? & 0xffff_ffff
        };
        // Zero-extend into XMM.
        regs.write_xmm(instr.op_register(0), u128::from(v as u32))?;
        return Ok(());
    }
    if instr.op1_register().is_xmm() {
        let v = regs.read_xmm(instr.op_register(1))? as u64 & 0xffff_ffff;
        if instr.op0_kind() == OpKind::Memory {
            write_mem_value(mem, effective_address(regs, instr)?, v, 4)?;
        } else {
            regs.write_reg(instr.op_register(0), v)?;
        }
        return Ok(());
    }
    Err(StepExecError::Cpu(CpuError::Message(
        "movd without xmm".into(),
    )))
}

/// `MOVHPS` — move 64 bits between XMM upper half and memory.
///
/// Two forms:
/// - `MOVHPS xmm, m64`  — load 8 bytes from m64 into xmm[127:64], low 64 unchanged
/// - `MOVHPS m64, xmm`  — store xmm[127:64] to m64
fn exec_sse_movhps(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    if instr.op0_register().is_xmm() {
        // xmm_dst, m64_src  → load into upper 64 bits
        let src = read_sse_op(mem, regs, instr, 1, 8)?; // 8 bytes from memory
        let old = regs.read_xmm(instr.op_register(0))?; // current XMM value
        let new = (old & u128::from(u64::MAX)) | (src << 64); // merge into upper half
        regs.write_xmm(instr.op_register(0), new)?;
    } else {
        // m64_dst, xmm_src  → store upper 64 bits to memory
        let src_reg = instr.op_register(1);
        let xmm_val = regs.read_xmm(src_reg)?;
        let upper = (xmm_val >> 64) as u64;
        write_sse_op(mem, regs, instr, 0, u128::from(upper), 8, false)?;
    }
    Ok(())
}

/// `MOVHLPS` / `MOVLHPS` — move packed floats between XMM upper/lower halves.
///
/// - `MOVHLPS xmm1, xmm2`: xmm1[63:0] = xmm2[127:64], xmm1[127:64] unchanged
/// - `MOVLHPS xmm1, xmm2`: xmm1[127:64] = xmm2[63:0], xmm1[63:0] unchanged
fn exec_sse_movhlps(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let dst_val = regs.read_xmm(dst)?;
    let src_val = regs.read_xmm(src)?;
    let new = match instr.mnemonic() {
        Mnemonic::Movhlps => {
            // upper 64 of src → lower 64 of dst, keep dst[127:64]
            (dst_val & !u128::from(u64::MAX)) | ((src_val >> 64) & u128::from(u64::MAX))
        }
        _ => {
            // Mnemonic::Movlhps:
            // lower 64 of src → upper 64 of dst, keep dst[63:0]
            (dst_val & u128::from(u64::MAX)) | ((src_val & u128::from(u64::MAX)) << 64)
        }
    };
    regs.write_xmm(dst, new)?;
    Ok(())
}

/// `PUNPCKLQDQ` / `PUNPCKHQDQ` — unpack and interleave quadwords from two XMM registers.
///
/// - `PUNPCKLQDQ xmm1, xmm2`: keep low half of dst, place low half of src in high half.
/// - `PUNPCKHQDQ xmm1, xmm2`: place high half of dst in low half, high half of src in high half.
fn exec_sse_punpck(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let a = regs.read_xmm(dst)?;
    let b = regs.read_xmm(src)?;
    let new = if instr.mnemonic() == Mnemonic::Punpcklqdq {
        (a & u128::from(u64::MAX)) | ((b & u128::from(u64::MAX)) << 64)
    } else {
        // Punpckhqdq
        ((a >> 64) & u128::from(u64::MAX)) | (b & !u128::from(u64::MAX))
    };
    regs.write_xmm(dst, new)?;
    Ok(())
}

/// `PSHUFD` — shuffle doublewords from XMM register (SSE2).
///
/// Copies four 32-bit lanes from `src` to `dst` according to an imm8
/// control byte at the end of the instruction encoding.
fn exec_sse_pshufd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src_val = read_sse_op(mem, regs, instr, 1, 16)?;
    let imm8 = instr.immediate(2) as u8;
    let lanes: Vec<u32> = (0..4)
        .map(|i| {
            let src_lane = ((imm8 >> (i * 2)) & 3) as usize;
            ((src_val >> (src_lane * 32)) & 0xffff_ffff) as u32
        })
        .collect();
    let result: u128 = lanes
        .into_iter()
        .enumerate()
        .fold(0u128, |acc, (i, lane)| acc | (u128::from(lane) << (i * 32)));
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `PSHUFLW` / `PSHUFHW` — shuffle the low/high 16-bit lanes of an XMM register.
fn exec_sse_pshuflw_hw(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src_val = read_sse_op(mem, regs, instr, 1, 16)?;
    let imm8 = instr.immediate(2) as u8;
    let low = instr.mnemonic() == Mnemonic::Pshuflw;
    // The untouched half (high words for pshuflw, low words for pshufhw) is
    // copied verbatim; only the other four words are permuted.
    let mut result = if low {
        src_val & (u128::from(u64::MAX) << 64)
    } else {
        src_val & u128::from(u64::MAX)
    };
    let base = if low { 0 } else { 4 };
    for i in 0..4 {
        let src_lane = ((imm8 >> (i * 2)) & 3) as usize;
        let from = if low { src_lane } else { 4 + src_lane };
        let word = ((src_val >> (from * 16)) & 0xffff) as u16;
        result |= u128::from(word) << ((base + i) * 16);
    }
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `PSHUFB` — byte-wise table lookup (SSSE3).
///
/// For each byte `i`: `dst[i] = (mask[i] & 0x80) ? 0 : table[mask[i] & 0x0F]`
/// with `table = dst (op0)` and `mask = src (op1)`.
fn exec_sse_pshufb(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let table = read_sse_op(mem, regs, instr, 0, 16)?;
    let mask = read_sse_op(mem, regs, instr, 1, 16)?;
    let result = u128::from(sse_pshufb_lo(
        table as u64,
        (table >> 64) as u64,
        mask as u64,
        (mask >> 64) as u64,
    )) | (u128::from(sse_pshufb_hi(
        table as u64,
        (table >> 64) as u64,
        mask as u64,
        (mask >> 64) as u64,
    )) << 64);
    write_sse_op(mem, regs, instr, 0, result, 16, false)
}

/// Packed integer binary op (arithmetic / compare / pack) on full 128 bits.
fn exec_sse_int_binop(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseIntOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    write_sse_op(mem, regs, instr, 0, sse_int_binop_u128(op, a, b), 16, false)
}

/// `Punpckl/H{bw,wd,dq}` — byte/word/dword unpack (the qdq variants use
/// [`exec_sse_punpck`], which is a pure half swap).
fn exec_sse_punpck_lanes(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let a_lo = a as u64;
    let a_hi = (a >> 64) as u64;
    let b_lo = b as u64;
    let b_hi = (b >> 64) as u64;
    // Both result halves come from the *same* source half: the L-family reads
    // the low 8/16 bytes of each operand (a_lo/b_lo), the H-family the high
    // bytes (a_hi/b_hi); the low result half interleaves the low sub-lanes and
    // the high result half the high sub-lanes.
    let (l_op, h_op, src_lo, src_hi) = match instr.mnemonic() {
        Mnemonic::Punpcklbw => (SseIntOp::Punpcklbw, SseIntOp::PunpckHiBw, a_lo, b_lo),
        Mnemonic::Punpcklwd => (SseIntOp::Punpcklwd, SseIntOp::PunpckHiWd, a_lo, b_lo),
        Mnemonic::Punpckldq => (SseIntOp::Punpckldq, SseIntOp::PunpckHiDq, a_lo, b_lo),
        Mnemonic::Punpckhbw => (SseIntOp::Punpcklbw, SseIntOp::PunpckHiBw, a_hi, b_hi),
        Mnemonic::Punpckhwd => (SseIntOp::Punpcklwd, SseIntOp::PunpckHiWd, a_hi, b_hi),
        Mnemonic::Punpckhdq => (SseIntOp::Punpckldq, SseIntOp::PunpckHiDq, a_hi, b_hi),
        _ => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "punpck lanes {:?}",
                instr.mnemonic()
            ))));
        }
    };
    let lo = sse_int_binop_half(l_op, src_lo, src_hi);
    let hi = sse_int_binop_half(h_op, src_lo, src_hi);
    write_sse_op(
        mem,
        regs,
        instr,
        0,
        u128::from(lo) | (u128::from(hi) << 64),
        16,
        false,
    )
}

/// Map a packed-integer mnemonic to its ABI opcode (arithmetic/compare/pack only).
fn sse_int_op(m: Mnemonic) -> SseIntOp {
    match m {
        Mnemonic::Paddb => SseIntOp::Paddb,
        Mnemonic::Paddw => SseIntOp::Paddw,
        Mnemonic::Paddd => SseIntOp::Paddd,
        Mnemonic::Paddq => SseIntOp::Paddq,
        Mnemonic::Psubb => SseIntOp::Psubb,
        Mnemonic::Psubw => SseIntOp::Psubw,
        Mnemonic::Psubd => SseIntOp::Psubd,
        Mnemonic::Psubq => SseIntOp::Psubq,
        Mnemonic::Paddsb => SseIntOp::Paddsb,
        Mnemonic::Paddsw => SseIntOp::Paddsw,
        Mnemonic::Paddusb => SseIntOp::Paddusb,
        Mnemonic::Paddusw => SseIntOp::Paddusw,
        Mnemonic::Psubsb => SseIntOp::Psubsb,
        Mnemonic::Psubsw => SseIntOp::Psubsw,
        Mnemonic::Psubusb => SseIntOp::Psubusb,
        Mnemonic::Psubusw => SseIntOp::Psubusw,
        Mnemonic::Pmullw => SseIntOp::Pmullw,
        Mnemonic::Pmulhw => SseIntOp::Pmulhw,
        Mnemonic::Pmulhuw => SseIntOp::Pmulhuw,
        Mnemonic::Pmuludq => SseIntOp::Pmuludq,
        Mnemonic::Pmaddwd => SseIntOp::Pmaddwd,
        Mnemonic::Pcmpeqb => SseIntOp::Pcmpeqb,
        Mnemonic::Pcmpeqw => SseIntOp::Pcmpeqw,
        Mnemonic::Pcmpeqd => SseIntOp::Pcmpeqd,
        Mnemonic::Pcmpgtb => SseIntOp::Pcmpgtb,
        Mnemonic::Pcmpgtw => SseIntOp::Pcmpgtw,
        Mnemonic::Pcmpgtd => SseIntOp::Pcmpgtd,
        Mnemonic::Packsswb => SseIntOp::Packsswb,
        Mnemonic::Packssdw => SseIntOp::Packssdw,
        Mnemonic::Packuswb => SseIntOp::Packuswb,
        _ => {
            // Unreachable: the dispatch arms only call `sse_int_op` for the
            // mnemonics enumerated above.
            SseIntOp::Paddb
        }
    }
}

/// Map a packed-shift mnemonic to its ABI opcode.
fn sse_shift_op(m: Mnemonic) -> SseShiftOp {
    match m {
        Mnemonic::Psllw => SseShiftOp::Psllw,
        Mnemonic::Pslld => SseShiftOp::Pslld,
        Mnemonic::Psllq => SseShiftOp::Psllq,
        Mnemonic::Psrlw => SseShiftOp::Psrlw,
        Mnemonic::Psrld => SseShiftOp::Psrld,
        Mnemonic::Psrlq => SseShiftOp::Psrlq,
        Mnemonic::Psraw => SseShiftOp::Psraw,
        Mnemonic::Psrad => SseShiftOp::Psrad,
        _ => {
            // Unreachable: the dispatch arms only call `sse_shift_op` for the
            // mnemonics enumerated above.
            SseShiftOp::Psllw
        }
    }
}

/// `Psll/Psrl/Psra{w,d,q}` — packed shifts (imm8 or variable XMM count).
fn exec_sse_shift(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseShiftOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let count = match instr.op_kind(1) {
        OpKind::Immediate8 | OpKind::Immediate16 | OpKind::Immediate32 => {
            let c = u64::from(instr.immediate(1) as u8);
            let width = match op {
                SseShiftOp::Psllw | SseShiftOp::Psrlw | SseShiftOp::Psraw => 16,
                SseShiftOp::Pslld | SseShiftOp::Psrld | SseShiftOp::Psrad => 32,
                SseShiftOp::Psllq | SseShiftOp::Psrlq => 64,
            };
            let splat = match width {
                16 => c * 0x0001_0001_0001_0001,
                32 => c * 0x0000_0001_0000_0001,
                _ => c,
            };
            u128::from(splat) | (u128::from(splat) << 64)
        }
        _ => read_sse_op(mem, regs, instr, 1, 16)?,
    };
    write_sse_op(mem, regs, instr, 0, sse_shift_u128(op, a, count), 16, false)
}

/// `Sqrtss/Sqrtsd` — scalar FP square root, merged into the destination's low lane.
fn exec_sse_sqrt_scalar(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let nbytes = if is_double { 8 } else { 4 };
    let src = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let op = if is_double {
        SseFpUnOp::Sqrtsd
    } else {
        SseFpUnOp::Sqrtss
    };
    let r = sse_fp_unop(op, src as u64);
    write_sse_op(mem, regs, instr, 0, u128::from(r), nbytes, true)
}

/// `Sqrtps/Sqrtpd` — packed FP square root (reads op1 only).
fn exec_sse_sqrt_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, 16)?;
    let op = if is_double {
        SseFpUnOp::Sqrtpd
    } else {
        SseFpUnOp::Sqrtps
    };
    let r = u128::from(sse_fp_unop(op, src as u64))
        | (u128::from(sse_fp_unop(op, (src >> 64) as u64)) << 64);
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

/// `Minss/Minsd/Maxss/Maxsd` — scalar FP min/max, merged into the low lane.
fn exec_sse_minmax_scalar(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseFpBinOp,
) -> Result<(), StepExecError> {
    let is_double = matches!(op, SseFpBinOp::Minsd | SseFpBinOp::Maxsd);
    let nbytes = if is_double { 8 } else { 4 };
    let a = read_sse_op(mem, regs, instr, 0, nbytes)?;
    let b = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let r = sse_fp_binop(op, a as u64, b as u64);
    write_sse_op(mem, regs, instr, 0, u128::from(r), nbytes, true)
}

/// `Minps/Maxps/Minpd/Maxpd` — packed FP min/max.
fn exec_sse_minmax_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseFpBinOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let r = u128::from(sse_fp_binop(op, a as u64, b as u64))
        | (u128::from(sse_fp_binop(op, (a >> 64) as u64, (b >> 64) as u64)) << 64);
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

/// `Comiss/Comisd/Ucomiss/Ucomisd` — compare FP, set ZF/PF/CF (OF/AF/SF cleared).
fn exec_sse_comis(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let nbytes = if is_double { 8 } else { 4 };
    let a = read_sse_op(mem, regs, instr, 0, nbytes)?;
    let b = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let (unordered, eq, lt) = if is_double {
        let fa = f64::from_bits(a as u64);
        let fb = f64::from_bits(b as u64);
        (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb)
    } else {
        let fa = f32::from_bits(a as u32);
        let fb = f32::from_bits(b as u32);
        (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb)
    };
    regs.set_flag(rflags::CF, lt || unordered);
    regs.set_flag(rflags::PF, unordered);
    regs.set_flag(rflags::ZF, eq || unordered);
    regs.set_flag(rflags::OF, false);
    regs.set_flag(rflags::AF, false);
    regs.set_flag(rflags::SF, false);
    Ok(())
}

/// `Cvtsi2ss/Cvtsi2sd` — signed integer → scalar FP, merged into the low lane.
fn exec_sse_cvt_gpr_to_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let is_double = instr.mnemonic() == Mnemonic::Cvtsi2sd;
    // 64-bit when the source is r64 or m64 (memory width is authoritative).
    let is64 = match instr.op1_kind() {
        OpKind::Memory => instr.memory_size().size() == 8,
        _ => instr.op_register(1).size() == 8,
    };
    let src = if instr.op1_kind() == OpKind::Memory {
        let addr = effective_address(regs, instr)?;
        let w = if is64 { 8 } else { 4 };
        read_mem_value(mem, addr, w)?
    } else {
        regs.read_reg(instr.op_register(1))?
    };
    let op = match (is_double, is64) {
        (false, false) => SseCvtOp::Cvtsi2ss32,
        (false, true) => SseCvtOp::Cvtsi2ss64,
        (true, false) => SseCvtOp::Cvtsi2sd32,
        (true, true) => SseCvtOp::Cvtsi2sd64,
    };
    let bits = sse_cvt(op, src);
    let nbytes = if is_double { 8 } else { 4 };
    write_sse_op(mem, regs, instr, 0, u128::from(bits), nbytes, true)
}

/// `Cvttss2si/Cvtss2si/Cvttsd2si/Cvtsd2si` — scalar FP → signed integer.
fn exec_sse_cvt_fp_to_gpr(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let is_double = matches!(instr.mnemonic(), Mnemonic::Cvttsd2si | Mnemonic::Cvtsd2si);
    let trunc = matches!(instr.mnemonic(), Mnemonic::Cvttss2si | Mnemonic::Cvttsd2si);
    let is64 = instr.op_register(0).size() == 8;
    let nbytes = if is_double { 8 } else { 4 };
    let a = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let op = match (is_double, is64, trunc) {
        (false, false, true) => SseCvtOp::Cvttss2si32,
        (false, false, false) => SseCvtOp::Cvtss2si32,
        (false, true, true) => SseCvtOp::Cvttss2si64,
        (false, true, false) => SseCvtOp::Cvtss2si64,
        (true, false, true) => SseCvtOp::Cvttsd2si32,
        (true, false, false) => SseCvtOp::Cvtsd2si32,
        (true, true, true) => SseCvtOp::Cvttsd2si64,
        (true, true, false) => SseCvtOp::Cvtsd2si64,
    };
    let v = sse_cvt(op, a as u64);
    regs.write_reg(instr.op_register(0), v)
        .map_err(StepExecError::Cpu)
}

/// `Cvtps2dq/Cvtdq2ps/Cvttps2dq` — packed FP ↔ int (reads op1 only).
fn exec_sse_cvt_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, 16)?;
    let op = match instr.mnemonic() {
        Mnemonic::Cvtps2dq => SseCvtOp::Cvtps2dq,
        Mnemonic::Cvtdq2ps => SseCvtOp::Cvtdq2ps,
        _ => SseCvtOp::Cvttps2dq,
    };
    let r =
        u128::from(sse_cvt(op, src as u64)) | (u128::from(sse_cvt(op, (src >> 64) as u64)) << 64);
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

/// `Unpcklpd` — unpack low packed double-precision floats (identical to punpcklqdq).
fn exec_sse_unpcklpd(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let a = regs.read_xmm(dst)?;
    let b = regs.read_xmm(src)?;
    // Low 64 bits from dst, low 64 bits from src.
    let result = (a & 0xffff_ffff_ffff_ffff) | ((b & 0xffff_ffff_ffff_ffff) << 64);
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `Psadbw` — sum of absolute differences of unsigned bytes.
/// Low 8 bytes → summed into lower 16 bits of lower qword.
/// High 8 bytes → summed into lower 16 bits of upper qword.
fn exec_sse_psadbw(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let low_sum: u64 = (0..8)
        .map(|i| {
            let shift = i * 8;
            let a_byte = ((a >> shift) & 0xff) as u64;
            let b_byte = ((b >> shift) & 0xff) as u64;
            a_byte.abs_diff(b_byte)
        })
        .sum();
    let high_sum: u64 = (0..8)
        .map(|i| {
            let shift = (i + 8) * 8;
            let a_byte = ((a >> shift) & 0xff) as u64;
            let b_byte = ((b >> shift) & 0xff) as u64;
            a_byte.abs_diff(b_byte)
        })
        .sum();
    let result = u128::from(low_sum) | (u128::from(high_sum) << 64);
    write_sse_op(mem, regs, instr, 0, result, 16, false)
}

// --- Packed integer SSE2: shared lane core (interpreter + JIT host helpers) ---
//
// Every op here is *half-aligned*: lanes of 1/2/4/8 bytes never straddle the
// u64 boundary of an XMM register, so the whole 128-bit op decomposes into two
// independent 64-bit halves. The JIT (with `WIE_JIT_SIMD=0`) and the iced
// interpreter therefore share one half-function per op; the JIT's vector path
// lowers the same ops to NEON directly.

/// ABI-stable packed-integer opcode (encoded into `iconst` immediates by the
/// JIT lowering). Values are part of the JIT ABI — never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseIntOp {
    Paddb = 0,
    Paddw = 1,
    Paddd = 2,
    Paddq = 3,
    Psubb = 4,
    Psubw = 5,
    Psubd = 6,
    Psubq = 7,
    Paddsb = 8,
    Paddsw = 9,
    Paddusb = 10,
    Paddusw = 11,
    Psubsb = 12,
    Psubsw = 13,
    Psubusb = 14,
    Psubusw = 15,
    Pmullw = 16,
    Pmulhw = 17,
    Pmulhuw = 18,
    Pmuludq = 19,
    Pmaddwd = 20,
    Pcmpeqb = 21,
    Pcmpeqw = 22,
    Pcmpeqd = 23,
    Pcmpgtb = 24,
    Pcmpgtw = 25,
    Pcmpgtd = 26,
    Packsswb = 27,
    Packssdw = 28,
    Packuswb = 29,
    Punpcklbw = 30,
    Punpcklwd = 31,
    Punpckldq = 32,
    /// High-sub-lane interleave used for the result's high half of `punpckh*`
    /// (the low half reuses the matching `Punpckl*` op over the high halves).
    PunpckHiBw = 33,
    PunpckHiWd = 34,
    PunpckHiDq = 35,
}

impl SseIntOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseIntOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Paddb,
            1 => Self::Paddw,
            2 => Self::Paddd,
            3 => Self::Paddq,
            4 => Self::Psubb,
            5 => Self::Psubw,
            6 => Self::Psubd,
            7 => Self::Psubq,
            8 => Self::Paddsb,
            9 => Self::Paddsw,
            10 => Self::Paddusb,
            11 => Self::Paddusw,
            12 => Self::Psubsb,
            13 => Self::Psubsw,
            14 => Self::Psubusb,
            15 => Self::Psubusw,
            16 => Self::Pmullw,
            17 => Self::Pmulhw,
            18 => Self::Pmulhuw,
            19 => Self::Pmuludq,
            20 => Self::Pmaddwd,
            21 => Self::Pcmpeqb,
            22 => Self::Pcmpeqw,
            23 => Self::Pcmpeqd,
            24 => Self::Pcmpgtb,
            25 => Self::Pcmpgtw,
            26 => Self::Pcmpgtd,
            27 => Self::Packsswb,
            28 => Self::Packssdw,
            29 => Self::Packuswb,
            30 => Self::Punpcklbw,
            31 => Self::Punpcklwd,
            32 => Self::Punpckldq,
            33 => Self::PunpckHiBw,
            34 => Self::PunpckHiWd,
            35 => Self::PunpckHiDq,
            _ => return Err(()),
        })
    }
}

/// ABI-stable packed-shift opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseShiftOp {
    Psllw = 0,
    Pslld = 1,
    Psllq = 2,
    Psrlw = 3,
    Psrld = 4,
    Psrlq = 5,
    Psraw = 6,
    Psrad = 7,
}

impl SseShiftOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseShiftOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Psllw,
            1 => Self::Pslld,
            2 => Self::Psllq,
            3 => Self::Psrlw,
            4 => Self::Psrld,
            5 => Self::Psrlq,
            6 => Self::Psraw,
            7 => Self::Psrad,
            _ => return Err(()),
        })
    }
}

/// ABI-stable packed/scalar FP unary opcode (sqrt family).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseFpUnOp {
    /// 2 f32 lanes per half.
    Sqrtps = 0,
    /// 1 f64 lane per half.
    Sqrtpd = 1,
    /// Scalar low f32 lane.
    Sqrtss = 2,
    /// Scalar low f64 lane.
    Sqrtsd = 3,
}

impl SseFpUnOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseFpUnOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Sqrtps,
            1 => Self::Sqrtpd,
            2 => Self::Sqrtss,
            3 => Self::Sqrtsd,
            _ => return Err(()),
        })
    }
}

/// ABI-stable FP min/max opcode (scalar + packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseFpBinOp {
    /// 2 f32 lanes per half.
    Minps = 0,
    Maxps = 1,
    /// 1 f64 lane per half.
    Minpd = 2,
    Maxpd = 3,
    /// Scalar low f32 lane.
    Minss = 4,
    Maxss = 5,
    /// Scalar low f64 lane.
    Minsd = 6,
    Maxsd = 7,
}

impl SseFpBinOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseFpBinOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Minps,
            1 => Self::Maxps,
            2 => Self::Minpd,
            3 => Self::Maxpd,
            4 => Self::Minss,
            5 => Self::Maxss,
            6 => Self::Minsd,
            7 => Self::Maxsd,
            _ => return Err(()),
        })
    }
}

/// ABI-stable convert opcode (GPR↔FP and packed FP↔int).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseCvtOp {
    Cvtsi2ss32 = 0,
    Cvtsi2ss64 = 1,
    Cvtsi2sd32 = 2,
    Cvtsi2sd64 = 3,
    Cvttss2si32 = 4,
    Cvtss2si32 = 5,
    Cvttss2si64 = 6,
    Cvtss2si64 = 7,
    Cvttsd2si32 = 8,
    Cvtsd2si32 = 9,
    Cvttsd2si64 = 10,
    Cvtsd2si64 = 11,
    /// 2 f32 lanes → 2 i32 lanes (round-nearest).
    Cvtps2dq = 12,
    /// 2 i32 lanes → 2 f32 lanes.
    Cvtdq2ps = 13,
    /// 2 f32 lanes → 2 i32 lanes (truncate).
    Cvttps2dq = 14,
}

impl SseCvtOp {
    pub(crate) fn to_abi(self) -> u64 {
        self as u64
    }
}

impl TryFrom<u64> for SseCvtOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Cvtsi2ss32,
            1 => Self::Cvtsi2ss64,
            2 => Self::Cvtsi2sd32,
            3 => Self::Cvtsi2sd64,
            4 => Self::Cvttss2si32,
            5 => Self::Cvtss2si32,
            6 => Self::Cvttss2si64,
            7 => Self::Cvtss2si64,
            8 => Self::Cvttsd2si32,
            9 => Self::Cvtsd2si32,
            10 => Self::Cvttsd2si64,
            11 => Self::Cvtsd2si64,
            12 => Self::Cvtps2dq,
            13 => Self::Cvtdq2ps,
            14 => Self::Cvttps2dq,
            _ => return Err(()),
        })
    }
}

/// Mask of the low `bits` bits of a u64 (all 64 for `bits >= 64`).
fn u64_mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1_u64 << bits) - 1
    }
}

/// Sign-extend the low `bits` bits of `v`.
fn sign_extend_bits(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        let sh = 64 - bits;
        ((v << sh) as i64) >> sh
    }
}

/// Apply `f` lane-wise over `bits`-wide lanes of two u64 halves.
fn sse_lane_binop<F>(a: u64, b: u64, bits: u32, f: F) -> u64
where
    F: Fn(u64, u64) -> u64,
{
    let lanes = 64 / bits;
    let mask = u64_mask(bits);
    let mut out = 0_u64;
    for i in 0..lanes {
        let sh = i * bits;
        let x = (a >> sh) & mask;
        let y = (b >> sh) & mask;
        out |= (f(x, y) & mask) << sh;
    }
    out
}

/// Apply `f` lane-wise where lane `i` of `count` is that lane's shift amount.
fn sse_lane_shift<F>(a: u64, count: u64, bits: u32, f: F) -> u64
where
    F: Fn(u64, u64) -> u64,
{
    sse_lane_binop(a, count, bits, f)
}

/// One u64 half of a packed integer binary op.
pub(crate) fn sse_int_binop_half(op: SseIntOp, a: u64, b: u64) -> u64 {
    match op {
        SseIntOp::Paddb => sse_lane_binop(a, b, 8, u64::wrapping_add),
        SseIntOp::Paddw => sse_lane_binop(a, b, 16, u64::wrapping_add),
        SseIntOp::Paddd => sse_lane_binop(a, b, 32, u64::wrapping_add),
        SseIntOp::Paddq => a.wrapping_add(b),
        SseIntOp::Psubb => sse_lane_binop(a, b, 8, u64::wrapping_sub),
        SseIntOp::Psubw => sse_lane_binop(a, b, 16, u64::wrapping_sub),
        SseIntOp::Psubd => sse_lane_binop(a, b, 32, u64::wrapping_sub),
        SseIntOp::Psubq => a.wrapping_sub(b),
        SseIntOp::Paddsb => sse_lane_binop(a, b, 8, |x, y| {
            u64::from(
                (sign_extend_bits(x, 8) as i8).saturating_add(sign_extend_bits(y, 8) as i8) as u8,
            )
        }),
        SseIntOp::Paddsw => sse_lane_binop(a, b, 16, |x, y| {
            u64::from(
                (sign_extend_bits(x, 16) as i16).saturating_add(sign_extend_bits(y, 16) as i16)
                    as u16,
            )
        }),
        SseIntOp::Paddusb => sse_lane_binop(a, b, 8, |x, y| (x + y).min(0xff)),
        SseIntOp::Paddusw => sse_lane_binop(a, b, 16, |x, y| (x + y).min(0xffff)),
        SseIntOp::Psubsb => sse_lane_binop(a, b, 8, |x, y| {
            u64::from(
                (sign_extend_bits(x, 8) as i8).saturating_sub(sign_extend_bits(y, 8) as i8) as u8,
            )
        }),
        SseIntOp::Psubsw => sse_lane_binop(a, b, 16, |x, y| {
            u64::from(
                (sign_extend_bits(x, 16) as i16).saturating_sub(sign_extend_bits(y, 16) as i16)
                    as u16,
            )
        }),
        SseIntOp::Psubusb => sse_lane_binop(a, b, 8, u64::saturating_sub),
        SseIntOp::Psubusw => sse_lane_binop(a, b, 16, u64::saturating_sub),
        SseIntOp::Pmullw => sse_lane_binop(a, b, 16, u64::wrapping_mul),
        SseIntOp::Pmulhw => sse_lane_binop(a, b, 16, |x, y| {
            ((sign_extend_bits(x, 16) * sign_extend_bits(y, 16)) >> 16) as u64
        }),
        SseIntOp::Pmulhuw => sse_lane_binop(a, b, 16, |x, y| x.wrapping_mul(y) >> 16),
        // Result qword i = low dword of each operand's qword i (half-aligned).
        SseIntOp::Pmuludq => (a & 0xffff_ffff).wrapping_mul(b & 0xffff_ffff),
        SseIntOp::Pmaddwd => {
            let mut out = 0_u64;
            for i in 0..2 {
                let w0 = sign_extend_bits((a >> (i * 32)) & 0xffff, 16);
                let w1 = sign_extend_bits((a >> (i * 32 + 16)) & 0xffff, 16);
                let x0 = sign_extend_bits((b >> (i * 32)) & 0xffff, 16);
                let x1 = sign_extend_bits((b >> (i * 32 + 16)) & 0xffff, 16);
                let r = w0.wrapping_mul(x0).wrapping_add(w1.wrapping_mul(x1));
                out |= ((r as u64) & 0xffff_ffff) << (i * 32);
            }
            out
        }
        SseIntOp::Pcmpeqb => sse_lane_binop(a, b, 8, |x, y| if x == y { 0xff } else { 0 }),
        SseIntOp::Pcmpeqw => sse_lane_binop(a, b, 16, |x, y| if x == y { 0xffff } else { 0 }),
        SseIntOp::Pcmpeqd => sse_lane_binop(a, b, 32, |x, y| if x == y { 0xffff_ffff } else { 0 }),
        SseIntOp::Pcmpgtb => sse_lane_binop(a, b, 8, |x, y| {
            if sign_extend_bits(x, 8) > sign_extend_bits(y, 8) {
                0xff
            } else {
                0
            }
        }),
        SseIntOp::Pcmpgtw => sse_lane_binop(a, b, 16, |x, y| {
            if sign_extend_bits(x, 16) > sign_extend_bits(y, 16) {
                0xffff
            } else {
                0
            }
        }),
        SseIntOp::Pcmpgtd => sse_lane_binop(a, b, 32, |x, y| {
            if sign_extend_bits(x, 32) > sign_extend_bits(y, 32) {
                0xffff_ffff
            } else {
                0
            }
        }),
        SseIntOp::Packsswb => {
            let sat = |w: u64| (sign_extend_bits(w, 16).clamp(-128, 127) as u64) & 0xff;
            let mut out = 0_u64;
            for i in 0..4 {
                out |= sat((a >> (i * 16)) & 0xffff) << (i * 8);
            }
            for i in 0..4 {
                out |= sat((b >> (i * 16)) & 0xffff) << ((i + 4) * 8);
            }
            out
        }
        SseIntOp::Packssdw => {
            let sat = |d: u64| (sign_extend_bits(d, 32).clamp(-32768, 32767) as u64) & 0xffff;
            let mut out = 0_u64;
            for i in 0..2 {
                out |= sat((a >> (i * 32)) & 0xffff_ffff) << (i * 16);
            }
            for i in 0..2 {
                out |= sat((b >> (i * 32)) & 0xffff_ffff) << ((i + 2) * 16);
            }
            out
        }
        SseIntOp::Packuswb => {
            let sat = |w: u64| (sign_extend_bits(w, 16).clamp(0, 255) as u64) & 0xff;
            let mut out = 0_u64;
            for i in 0..4 {
                out |= sat((a >> (i * 16)) & 0xffff) << (i * 8);
            }
            for i in 0..4 {
                out |= sat((b >> (i * 16)) & 0xffff) << ((i + 4) * 8);
            }
            out
        }
        SseIntOp::Punpcklbw
        | SseIntOp::Punpcklwd
        | SseIntOp::Punpckldq
        | SseIntOp::PunpckHiBw
        | SseIntOp::PunpckHiWd
        | SseIntOp::PunpckHiDq => {
            let bits = match op {
                SseIntOp::Punpcklbw | SseIntOp::PunpckHiBw => 8,
                SseIntOp::Punpcklwd | SseIntOp::PunpckHiWd => 16,
                _ => 32,
            };
            let high = matches!(
                op,
                SseIntOp::PunpckHiBw | SseIntOp::PunpckHiWd | SseIntOp::PunpckHiDq
            );
            sse_punpck_half(a, b, bits, high)
        }
    }
}

/// Interleave the low (`high == false`) or high (`high == true`) sub-lanes of
/// two u64 halves, producing one u64 of the unpacked result.
fn sse_punpck_half(a: u64, b: u64, bits: u32, high: bool) -> u64 {
    let lanes = (64 / bits) / 2;
    let mask = u64_mask(bits);
    let src_shift = if high { lanes } else { 0 };
    let mut out = 0_u64;
    for i in 0..lanes {
        let x = (a >> ((src_shift + i) * bits)) & mask;
        let y = (b >> ((src_shift + i) * bits)) & mask;
        out |= x << (2 * i * bits);
        out |= y << ((2 * i + 1) * bits);
    }
    out
}

/// One u64 half of a packed shift. `count` holds per-lane counts (the JIT
/// splats an imm8 into every lane for the immediate forms). Counts at or above
/// the element width produce 0 (x86 PSLL/PSRL/PSRA semantics).
pub(crate) fn sse_shift_half(op: SseShiftOp, a: u64, count: u64) -> u64 {
    match op {
        SseShiftOp::Psllw => sse_lane_shift(a, count, 16, |x, c| {
            if c >= 16 { 0 } else { x.wrapping_shl(c as u32) }
        }),
        SseShiftOp::Pslld => sse_lane_shift(a, count, 32, |x, c| {
            if c >= 32 { 0 } else { x.wrapping_shl(c as u32) }
        }),
        SseShiftOp::Psllq => sse_lane_shift(a, count, 64, |x, c| {
            if c >= 64 { 0 } else { x.wrapping_shl(c as u32) }
        }),
        SseShiftOp::Psrlw => sse_lane_shift(a, count, 16, |x, c| {
            if c >= 16 { 0 } else { x.wrapping_shr(c as u32) }
        }),
        SseShiftOp::Psrld => sse_lane_shift(a, count, 32, |x, c| {
            if c >= 32 { 0 } else { x.wrapping_shr(c as u32) }
        }),
        SseShiftOp::Psrlq => sse_lane_shift(a, count, 64, |x, c| {
            if c >= 64 { 0 } else { x.wrapping_shr(c as u32) }
        }),
        SseShiftOp::Psraw => sse_lane_shift(a, count, 16, |x, c| {
            if c >= 16 {
                0
            } else {
                (sign_extend_bits(x, 16) >> c) as u64
            }
        }),
        SseShiftOp::Psrad => sse_lane_shift(a, count, 32, |x, c| {
            if c >= 32 {
                0
            } else {
                (sign_extend_bits(x, 32) >> c) as u64
            }
        }),
    }
}

/// Byte-index from a pshufb control byte (low nibble selects the table byte;
/// bit 7 means the result byte is 0).
fn pshufb_src_index(mi: u8) -> Option<u32> {
    if mi & 0x80 != 0 {
        None
    } else {
        Some(u32::from(mi & 0x0f))
    }
}

/// Result bytes 0..8 of `pshufb table, mask` (table = a, mask = b).
pub(crate) fn sse_pshufb_lo(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    let table = [a_lo, a_hi];
    let mask = [b_lo, b_hi];
    let mut out = 0_u64;
    for i in 0..8 {
        let mi = ((mask[i / 8] >> ((i % 8) * 8)) & 0xff) as u8;
        let byte = match pshufb_src_index(mi) {
            Some(j) => ((table[usize::try_from(j / 8).unwrap_or(0)] >> ((j % 8) * 8)) & 0xff) as u8,
            None => 0,
        };
        out |= u64::from(byte) << (i * 8);
    }
    out
}

/// Result bytes 8..16 of `pshufb table, mask`.
pub(crate) fn sse_pshufb_hi(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    let table = [a_lo, a_hi];
    let mask = [b_lo, b_hi];
    let mut out = 0_u64;
    for i in 8..16 {
        let mi = ((mask[i / 8] >> ((i % 8) * 8)) & 0xff) as u8;
        let byte = match pshufb_src_index(mi) {
            Some(j) => ((table[usize::try_from(j / 8).unwrap_or(0)] >> ((j % 8) * 8)) & 0xff) as u8,
            None => 0,
        };
        out |= u64::from(byte) << ((i - 8) * 8);
    }
    out
}

/// x86 MINSS/MINPS semantics: if either operand is NaN the source (`b`) wins
/// (Rust's `f32::min`/`max` return the non-NaN operand instead).
fn sse_fp_minmax32(a: f32, b: f32, want_min: bool) -> f32 {
    if a.is_nan() || b.is_nan() {
        return b;
    }
    if want_min {
        if a < b { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

fn sse_fp_minmax64(a: f64, b: f64, want_min: bool) -> f64 {
    if a.is_nan() || b.is_nan() {
        return b;
    }
    if want_min {
        if a < b { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

/// Packed/scalar FP unary (sqrt family) on one u64 half.
pub(crate) fn sse_fp_unop(op: SseFpUnOp, a: u64) -> u64 {
    match op {
        SseFpUnOp::Sqrtps => {
            let f0 = f32::from_bits((a & 0xffff_ffff) as u32).sqrt();
            let f1 = f32::from_bits(((a >> 32) & 0xffff_ffff) as u32).sqrt();
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << 32)
        }
        SseFpUnOp::Sqrtpd | SseFpUnOp::Sqrtsd => f64::from_bits(a).sqrt().to_bits(),
        SseFpUnOp::Sqrtss => u64::from(f32::from_bits((a & 0xffff_ffff) as u32).sqrt().to_bits()),
    }
}

/// Packed/scalar FP min/max on one u64 half.
pub(crate) fn sse_fp_binop(op: SseFpBinOp, a: u64, b: u64) -> u64 {
    match op {
        SseFpBinOp::Minps | SseFpBinOp::Maxps => {
            let want_min = op == SseFpBinOp::Minps;
            let f0 = sse_fp_minmax32(
                f32::from_bits((a & 0xffff_ffff) as u32),
                f32::from_bits((b & 0xffff_ffff) as u32),
                want_min,
            );
            let f1 = sse_fp_minmax32(
                f32::from_bits(((a >> 32) & 0xffff_ffff) as u32),
                f32::from_bits(((b >> 32) & 0xffff_ffff) as u32),
                want_min,
            );
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << 32)
        }
        SseFpBinOp::Minpd | SseFpBinOp::Maxpd => sse_fp_minmax64(
            f64::from_bits(a),
            f64::from_bits(b),
            op == SseFpBinOp::Minpd,
        )
        .to_bits(),
        SseFpBinOp::Minss | SseFpBinOp::Maxss => u64::from(
            sse_fp_minmax32(
                f32::from_bits((a & 0xffff_ffff) as u32),
                f32::from_bits((b & 0xffff_ffff) as u32),
                op == SseFpBinOp::Minss,
            )
            .to_bits(),
        ),
        SseFpBinOp::Minsd | SseFpBinOp::Maxsd => sse_fp_minmax64(
            f64::from_bits(a),
            f64::from_bits(b),
            op == SseFpBinOp::Minsd,
        )
        .to_bits(),
    }
}

fn f32_to_i32_trunc(f: f32) -> i32 {
    if f.is_nan() || f >= 2_147_483_648.0 || f < -2_147_483_648.0 {
        return i32::MIN;
    }
    f.trunc() as i32
}

fn f32_to_i32_round(f: f32) -> i32 {
    if f.is_nan() || f >= 2_147_483_648.0 || f < -2_147_483_648.0 {
        return i32::MIN;
    }
    f.round_ties_even() as i32
}

fn f32_to_i64_trunc(f: f32) -> i64 {
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    f.trunc() as i64
}

fn f32_to_i64_round(f: f32) -> i64 {
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    f.round_ties_even() as i64
}

fn f64_to_i32_trunc(f: f64) -> i32 {
    if f.is_nan() || f >= 2_147_483_648.0 || f < -2_147_483_648.0 {
        return i32::MIN;
    }
    f.trunc() as i32
}

fn f64_to_i32_round(f: f64) -> i32 {
    if f.is_nan() || f >= 2_147_483_648.0 || f < -2_147_483_648.0 {
        return i32::MIN;
    }
    f.round_ties_even() as i32
}

fn f64_to_i64_trunc(f: f64) -> i64 {
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    f.trunc() as i64
}

fn f64_to_i64_round(f: f64) -> i64 {
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    f.round_ties_even() as i64
}

/// Convert on one u64 half. `a` is the raw input bits (GPR value, FP bits, or
/// a half holding two 32-bit lanes for the packed forms).
pub(crate) fn sse_cvt(op: SseCvtOp, a: u64) -> u64 {
    match op {
        SseCvtOp::Cvtsi2ss32 => u64::from(((a as u32 as i32) as f32).to_bits()),
        SseCvtOp::Cvtsi2ss64 => u64::from(((a as i64) as f32).to_bits()),
        SseCvtOp::Cvtsi2sd32 => f64::from(a as u32 as i32).to_bits(),
        SseCvtOp::Cvtsi2sd64 => ((a as i64) as f64).to_bits(),
        SseCvtOp::Cvttss2si32 => u64::from(f32_to_i32_trunc(f32::from_bits(a as u32)) as u32),
        SseCvtOp::Cvtss2si32 => u64::from(f32_to_i32_round(f32::from_bits(a as u32)) as u32),
        SseCvtOp::Cvttss2si64 => f32_to_i64_trunc(f32::from_bits(a as u32)) as u64,
        SseCvtOp::Cvtss2si64 => f32_to_i64_round(f32::from_bits(a as u32)) as u64,
        SseCvtOp::Cvttsd2si32 => u64::from(f64_to_i32_trunc(f64::from_bits(a)) as u32),
        SseCvtOp::Cvtsd2si32 => u64::from(f64_to_i32_round(f64::from_bits(a)) as u32),
        SseCvtOp::Cvttsd2si64 => f64_to_i64_trunc(f64::from_bits(a)) as u64,
        SseCvtOp::Cvtsd2si64 => f64_to_i64_round(f64::from_bits(a)) as u64,
        SseCvtOp::Cvtps2dq => {
            let d0 = f32_to_i32_round(f32::from_bits((a & 0xffff_ffff) as u32)) as u32;
            let d1 = f32_to_i32_round(f32::from_bits(((a >> 32) & 0xffff_ffff) as u32)) as u32;
            u64::from(d0) | (u64::from(d1) << 32)
        }
        SseCvtOp::Cvttps2dq => {
            let d0 = f32_to_i32_trunc(f32::from_bits((a & 0xffff_ffff) as u32)) as u32;
            let d1 = f32_to_i32_trunc(f32::from_bits(((a >> 32) & 0xffff_ffff) as u32)) as u32;
            u64::from(d0) | (u64::from(d1) << 32)
        }
        SseCvtOp::Cvtdq2ps => {
            let f0 = (a as u32 as i32) as f32;
            let f1 = ((a >> 32) as u32 as i32) as f32;
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << 32)
        }
    }
}

/// Full 128-bit packed integer binop (interpreter path).
fn sse_int_binop_u128(op: SseIntOp, a: u128, b: u128) -> u128 {
    u128::from(sse_int_binop_half(op, a as u64, b as u64))
        | (u128::from(sse_int_binop_half(op, (a >> 64) as u64, (b >> 64) as u64)) << 64)
}

/// Full 128-bit packed shift (interpreter path). For the imm forms the caller
/// splats the count into every lane.
fn sse_shift_u128(op: SseShiftOp, a: u128, count: u128) -> u128 {
    u128::from(sse_shift_half(op, a as u64, count as u64))
        | (u128::from(sse_shift_half(op, (a >> 64) as u64, (count >> 64) as u64)) << 64)
}

fn read_sse_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    nbytes: usize,
) -> Result<u128, StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register if instr.op_register(op).is_xmm() => {
            let v = regs.read_xmm(instr.op_register(op))?;
            let mask = if nbytes >= 16 {
                u128::MAX
            } else {
                (1u128 << (nbytes.saturating_mul(8))) - 1
            };
            Ok(v & mask)
        }
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            let mut buf = [0_u8; 16];
            let slice = buf
                .get_mut(..nbytes)
                .ok_or_else(|| StepExecError::Cpu(CpuError::Message("sse read size".into())))?;
            if let Err(e) = mem.read(addr, slice) {
                drop(e);
                return Err(StepExecError::InvalidMemory(InvalidMem {
                    access_type: ACCESS_READ,
                    address: addr,
                    size: i32::try_from(nbytes).unwrap_or(0),
                    value: 0,
                }));
            }
            let mut v = 0_u128;
            for (i, b) in slice.iter().enumerate() {
                v |= u128::from(*b) << (i.saturating_mul(8));
            }
            Ok(v)
        }
        OpKind::Register => {
            // GPR source for movd/movq-like
            Ok(u128::from(regs.read_reg(instr.op_register(op))?))
        }
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "sse read op kind {other:?}"
        )))),
    }
}

fn write_sse_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    value: u128,
    nbytes: usize,
    scalar_merge: bool,
) -> Result<(), StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register if instr.op_register(op).is_xmm() => {
            let reg = instr.op_register(op);
            let new = if scalar_merge && nbytes < 16 {
                let old = regs.read_xmm(reg)?;
                let mask = (1u128 << (nbytes.saturating_mul(8))) - 1;
                (old & !mask) | (value & mask)
            } else if nbytes >= 16 {
                value
            } else {
                // Zero upper bits for full vector store of partial (non-merge).
                value & ((1u128 << (nbytes.saturating_mul(8))) - 1)
            };
            // For movsd/movss scalar to xmm: merge low bits, keep upper (SSE legacy).
            // For movaps full: replace all.
            let final_v = if scalar_merge {
                new
            } else if nbytes < 16 {
                value & ((1u128 << (nbytes.saturating_mul(8))) - 1)
            } else {
                value
            };
            regs.write_xmm(reg, final_v)?;
            Ok(())
        }
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            let mut buf = [0_u8; 16];
            for i in 0..nbytes {
                if let Some(b) = buf.get_mut(i) {
                    *b = ((value >> (i.saturating_mul(8))) & 0xff) as u8;
                }
            }
            let slice = buf
                .get(..nbytes)
                .ok_or_else(|| StepExecError::Cpu(CpuError::Message("sse write size".into())))?;
            if let Err(e) = mem.write(addr, slice) {
                drop(e);
                return Err(StepExecError::InvalidMemory(InvalidMem {
                    access_type: ACCESS_WRITE,
                    address: addr,
                    size: i32::try_from(nbytes).unwrap_or(0),
                    value: 0,
                }));
            }
            Ok(())
        }
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "sse write op kind {other:?}"
        )))),
    }
}

/// REP / REPE / REPNE present (F2/F3 string prefixes).
fn has_any_rep(instr: &Instruction) -> bool {
    instr.has_rep_prefix() || instr.has_repe_prefix() || instr.has_repne_prefix()
}

fn df_step(regs: &RegFile, size: usize) -> i64 {
    let s = i64::try_from(size).unwrap_or(1);
    if regs.flag(rflags::DF) { -s } else { s }
}

/// Keep RIP on a REP-prefixed string insn (Unicorn `count=1` micro-step).
///
/// Unicorn/QEMU semantics observed for `emu_start(..., count=1)`:
/// - One string iteration per counted step.
/// - After a **productive** iteration, RIP stays on the insn unless REPE/REPNE
///   stops early via ZF (then RIP advances).
/// - RCX exhausting to 0 does **not** advance RIP; the next step is a RCX=0
///   no-op that finally falls through.
/// - Entering with RCX=0 is a pure no-op that advances RIP (handled by callers
///   returning with the fall-through RIP already set in `execute_one`).
fn apply_string_stay(regs: &mut RegFile, instr: &Instruction, stay: bool) {
    if stay {
        regs.rip = instr.ip();
    }
}

/// String op kind for interpreter + JIT host helper.
///
/// The discriminants are the ABI contract with [`crate::jit::wie_jit_string`]:
/// the JIT passes the kind through an `extern "C"` `u64` parameter, so it is
/// encoded via [`Self::to_abi`] and decoded via [`TryFrom<u64>`]. Those two are
/// the *only* places the numeric form should appear — everything else works on
/// the enum, so a mis-typed literal cannot silently select the wrong operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StringOpKind {
    Stos = 0,
    Movs = 1,
    Lods = 2,
    Scas = 3,
    Cmps = 4,
}

impl StringOpKind {
    /// Classify a string mnemonic, returning the kind and its element size in
    /// bytes. `None` for any non-string mnemonic.
    ///
    /// Note `Movsd` is ambiguous in iced: the string form (`movsd`, 4-byte move)
    /// shares a mnemonic with the SSE scalar-double form. Only the string form
    /// reaches here, so it is classified as a 4-byte `Movs`.
    pub(crate) fn from_mnemonic(m: Mnemonic, size: u32) -> Option<(Self, u32)> {
        let out = match m {
            Mnemonic::Stosb | Mnemonic::Stosw | Mnemonic::Stosd | Mnemonic::Stosq => {
                (Self::Stos, size)
            }
            Mnemonic::Movsb | Mnemonic::Movsw | Mnemonic::Movsq => (Self::Movs, size),
            Mnemonic::Movsd => (Self::Movs, 4),
            Mnemonic::Lodsb | Mnemonic::Lodsd | Mnemonic::Lodsq => (Self::Lods, size),
            Mnemonic::Scasb | Mnemonic::Scasw | Mnemonic::Scasd | Mnemonic::Scasq => {
                (Self::Scas, size)
            }
            Mnemonic::Cmpsb | Mnemonic::Cmpsw | Mnemonic::Cmpsd | Mnemonic::Cmpsq => {
                (Self::Cmps, size)
            }
            _ => return None,
        };
        Some(out)
    }

    /// Encode for the `extern "C"` JIT helper ABI.
    pub(crate) fn to_abi(self) -> u64 {
        match self {
            Self::Stos => 0,
            Self::Movs => 1,
            Self::Lods => 2,
            Self::Scas => 3,
            Self::Cmps => 4,
        }
    }
}

impl TryFrom<u64> for StringOpKind {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Stos),
            1 => Ok(Self::Movs),
            2 => Ok(Self::Lods),
            3 => Ok(Self::Scas),
            4 => Ok(Self::Cmps),
            _ => Err(()),
        }
    }
}

/// REP-family prefixes on a string instruction.
///
/// Replaces a hand-packed `bit0=rep, bit1=repe, bit2=repne` word that was
/// encoded in the JIT lowering and decoded in the host helper — two copies of
/// the same magic layout that had to be kept in sync by hand.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct RepPrefix {
    /// Any of REP / REPE / REPNE is present (drives the iteration loop).
    pub rep: bool,
    /// REPE / REPZ — SCAS and CMPS exit early when ZF clears.
    pub repe: bool,
    /// REPNE / REPNZ — SCAS and CMPS exit early when ZF sets.
    pub repne: bool,
}

impl RepPrefix {
    pub(crate) fn from_instr(instr: &Instruction) -> Self {
        let repe = instr.has_repe_prefix();
        let repne = instr.has_repne_prefix();
        Self {
            // `has_rep_prefix` and `has_repe_prefix` alias the same F3 byte in
            // iced, so REP presence is the union of all three.
            rep: instr.has_rep_prefix() || repe || repne,
            repe,
            repne,
        }
    }

    /// Encode for the `extern "C"` JIT helper ABI.
    pub(crate) fn to_abi(self) -> u64 {
        u64::from(self.rep) | (u64::from(self.repe) << 1) | (u64::from(self.repne) << 2)
    }

    pub(crate) fn from_abi(bits: u64) -> Self {
        Self {
            rep: (bits & 1) != 0,
            repe: (bits & 2) != 0,
            repne: (bits & 4) != 0,
        }
    }
}

/// Bulk string op shared by iced and JIT. Returns `true` if RIP should stay on the insn.
pub(crate) fn run_string_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    kind: StringOpKind,
    size: usize,
    rep: RepPrefix,
) -> Result<bool, StepExecError> {
    match kind {
        StringOpKind::Stos => string_stos(mem, regs, size, rep.rep),
        StringOpKind::Movs => string_movs(mem, regs, size, rep.rep),
        StringOpKind::Lods => string_lods(mem, regs, size, rep.rep),
        StringOpKind::Scas => string_scas(mem, regs, size, rep.rep, rep.repe, rep.repne),
        StringOpKind::Cmps => string_cmps(mem, regs, size, rep.rep, rep.repe, rep.repne),
    }
}

/// Max elements processed in one bulk REP string step (faults still leave partial state).
const REP_BULK_MAX: u64 = 1 << 20;

/// Minimum byte length for host-span `memcpy`/`memset` (Phase 4.3).
///
/// Smaller REPs stay on the existing page-chunked `GuestMemory::{read,write}` path.
const REP_HOST_BULK_MIN_BYTES: usize = 16;

/// Whether REP MOVS/STOS may use soft-translated host `memcpy`/`memset`.
///
/// Kill-switch: `WIE_STRING_BULK=0|off|slow` forces the page-chunked path only.
fn string_host_bulk_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("WIE_STRING_BULK"),
            Ok(v)
                if v == "0"
                    || v.eq_ignore_ascii_case("off")
                    || v.eq_ignore_ascii_case("slow")
                    || v.eq_ignore_ascii_case("false")
        )
    })
}

/// Host-span fill for REP STOS (pattern width 1/2/4/8).
///
/// # Safety
/// `host` must point to `len` writable host bytes from [`GuestMemory::host_span`].
#[expect(unsafe_code)]
unsafe fn host_fill_pattern(host: *mut u8, len: usize, val: u64, size: usize) {
    if size == 0 || len == 0 {
        return;
    }
    if size == 1 {
        // SAFETY: caller guarantees `host`/`len` from soft-translated span.
        #[expect(unsafe_code)]
        unsafe {
            std::ptr::write_bytes(host, u8::try_from(val & 0xff).unwrap_or(0), len);
        }
        return;
    }
    let bytes = val.to_le_bytes();
    let pat = &bytes[..size.min(8)];
    let mut i = 0_usize;
    while i.saturating_add(size) <= len {
        // SAFETY: `i + size <= len`; host span is live for this call.
        #[expect(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(pat.as_ptr(), host.add(i), size);
        }
        i = i.saturating_add(size);
    }
}

fn exec_stos(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    size: usize,
) -> Result<(), StepExecError> {
    let stay = string_stos(mem, regs, size, has_any_rep(instr))?;
    apply_string_stay(regs, instr, stay);
    Ok(())
}

fn string_stos(
    mem: &GuestMemory,
    regs: &mut RegFile,
    size: usize,
    rep: bool,
) -> Result<bool, StepExecError> {
    if rep && regs.rcx() == 0 {
        return Ok(false);
    }
    let step = df_step(regs, size);
    let val = regs.rax() & regs::size_mask(size);
    if rep {
        let mut count = regs.rcx().min(REP_BULK_MAX);
        let mut rdi = regs.rdi();
        let size_u = u64::try_from(size).unwrap_or(1);
        // Phase 4.3: soft-translated host span → memset-like fill (DF=0 or DF=1).
        if count > 1 && string_host_bulk_enabled() {
            let byte_len_u = count.saturating_mul(size_u);
            let byte_len = usize::try_from(byte_len_u).unwrap_or(0);
            if byte_len >= REP_HOST_BULK_MIN_BYTES {
                // DF=0: [rdi, rdi+len). DF=1: last store at rdi-(count-1)*size.
                let span_base = if step > 0 {
                    Some(rdi)
                } else {
                    let last_off = (count - 1).saturating_mul(size_u);
                    rdi.checked_sub(last_off)
                };
                if let Some(base) = span_base
                    && let Some(host) = mem.host_span(base, byte_len, true)
                {
                    // SAFETY: host_span checked SPC + contiguous host mapping.
                    #[expect(unsafe_code)]
                    unsafe {
                        host_fill_pattern(host, byte_len, val, size);
                    }
                    let delta = if step > 0 {
                        byte_len_u
                    } else {
                        byte_len_u.wrapping_neg()
                    };
                    regs.set_rdi(rdi.wrapping_add(delta));
                    regs.set_rcx(regs.rcx().saturating_sub(count));
                    return Ok(true);
                }
            }
        }
        // Forward DF: page-chunked mem.write with a repeated pattern.
        if step > 0 && count > 1 {
            let mut buf = vec![0_u8; 4096];
            fill_pattern(&mut buf, val, size);
            let mut done_elems = 0_u64;
            while done_elems < count {
                let remain_elems = count - done_elems;
                let remain_bytes =
                    usize::try_from(remain_elems.saturating_mul(size_u)).unwrap_or(0);
                let chunk = remain_bytes.min(buf.len());
                let aligned = chunk - (chunk % size.max(1));
                if aligned == 0 {
                    break;
                }
                if let Err(e) = mem.write(rdi, &buf[..aligned]) {
                    drop(e);
                    regs.set_rdi(rdi);
                    regs.set_rcx(regs.rcx().saturating_sub(done_elems));
                    write_mem_value(mem, rdi, val, size)?;
                    return Ok(false);
                }
                let elems = u64::try_from(aligned / size).unwrap_or(0);
                rdi = rdi.wrapping_add(elems.saturating_mul(size_u));
                done_elems = done_elems.saturating_add(elems);
            }
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(done_elems));
            return Ok(done_elems > 0);
        }
        while count > 0 {
            write_mem_value(mem, rdi, val, size)?;
            rdi = rdi.wrapping_add(step as u64);
            count = count.saturating_sub(1);
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(1));
        }
        return Ok(true);
    }
    let rdi = regs.rdi();
    write_mem_value(mem, rdi, val, size)?;
    regs.set_rdi(rdi.wrapping_add(step as u64));
    Ok(false)
}

fn fill_pattern(buf: &mut [u8], val: u64, size: usize) {
    let bytes = val.to_le_bytes();
    let pat = &bytes[..size.min(8)];
    let mut i = 0;
    while i + size <= buf.len() {
        buf[i..i + size].copy_from_slice(pat);
        i += size;
    }
}

fn exec_movs(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    size: usize,
) -> Result<(), StepExecError> {
    let stay = string_movs(mem, regs, size, has_any_rep(instr))?;
    apply_string_stay(regs, instr, stay);
    Ok(())
}

fn string_movs(
    mem: &GuestMemory,
    regs: &mut RegFile,
    size: usize,
    rep: bool,
) -> Result<bool, StepExecError> {
    if rep && regs.rcx() == 0 {
        return Ok(false);
    }
    let step = df_step(regs, size);
    if rep {
        let mut count = regs.rcx().min(REP_BULK_MAX);
        let mut rsi = regs.rsi();
        let mut rdi = regs.rdi();
        let size_u = u64::try_from(size).unwrap_or(1);
        let byte_len_u = count.saturating_mul(size_u);
        let byte_len = usize::try_from(byte_len_u).unwrap_or(0);
        // Lowest address of each span (DF=1 starts at the high end).
        let last_off = (count.saturating_sub(1)).saturating_mul(size_u);
        let (src_lo, dst_lo) = if step > 0 {
            (rsi, rdi)
        } else {
            (rsi.wrapping_sub(last_off), rdi.wrapping_sub(last_off))
        };
        let overlap = ranges_overlap(src_lo, dst_lo, byte_len_u);
        // Phase 4.3: non-overlapping guest ranges + soft-translated host spans
        // → `copy_nonoverlapping`. Guest-overlapping REP MOVS stays on the
        // element loop (x86 directional copy ≠ host `memmove`).
        if count > 1
            && !overlap
            && byte_len >= REP_HOST_BULK_MIN_BYTES
            && string_host_bulk_enabled()
        {
            let src = mem.host_span(src_lo, byte_len, false);
            let dst = mem.host_span(dst_lo, byte_len, true);
            if let (Some(src_p), Some(dst_p)) = (src, dst)
                && !host_ranges_overlap(src_p, dst_p, byte_len)
            {
                // SAFETY: both spans passed SPC; same len; no overlap; live.
                #[expect(unsafe_code)]
                unsafe {
                    std::ptr::copy_nonoverlapping(src_p, dst_p, byte_len);
                }
                let delta = if step > 0 {
                    byte_len_u
                } else {
                    byte_len_u.wrapping_neg()
                };
                regs.set_rsi(rsi.wrapping_add(delta));
                regs.set_rdi(rdi.wrapping_add(delta));
                regs.set_rcx(regs.rcx().saturating_sub(count));
                return Ok(true);
            }
        }
        // Page-chunked path only for forward DF + non-overlap (existing).
        if step > 0 && count > 1 && !overlap {
            let mut buf = vec![0_u8; 4096];
            let mut done_elems = 0_u64;
            while done_elems < count {
                let remain_elems = count - done_elems;
                let remain_bytes =
                    usize::try_from(remain_elems.saturating_mul(size_u)).unwrap_or(0);
                let chunk = remain_bytes.min(buf.len());
                let aligned = chunk - (chunk % size.max(1));
                if aligned == 0 {
                    break;
                }
                if let Err(e) = mem.read(rsi, &mut buf[..aligned]) {
                    drop(e);
                    regs.set_rsi(rsi);
                    regs.set_rdi(rdi);
                    regs.set_rcx(regs.rcx().saturating_sub(done_elems));
                    let v = read_mem_value(mem, rsi, size)?;
                    write_mem_value(mem, rdi, v, size)?;
                    return Ok(false);
                }
                if let Err(e) = mem.write(rdi, &buf[..aligned]) {
                    drop(e);
                    regs.set_rsi(rsi);
                    regs.set_rdi(rdi);
                    regs.set_rcx(regs.rcx().saturating_sub(done_elems));
                    let v = read_mem_value(mem, rsi, size)?;
                    write_mem_value(mem, rdi, v, size)?;
                    return Ok(false);
                }
                let elems = u64::try_from(aligned / size).unwrap_or(0);
                let delta = elems.saturating_mul(size_u);
                rsi = rsi.wrapping_add(delta);
                rdi = rdi.wrapping_add(delta);
                done_elems = done_elems.saturating_add(elems);
            }
            regs.set_rsi(rsi);
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(done_elems));
            return Ok(done_elems > 0);
        }
        while count > 0 {
            let v = read_mem_value(mem, rsi, size)?;
            write_mem_value(mem, rdi, v, size)?;
            rsi = rsi.wrapping_add(step as u64);
            rdi = rdi.wrapping_add(step as u64);
            count = count.saturating_sub(1);
            regs.set_rsi(rsi);
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(1));
        }
        return Ok(true);
    }
    let rsi = regs.rsi();
    let rdi = regs.rdi();
    let v = read_mem_value(mem, rsi, size)?;
    write_mem_value(mem, rdi, v, size)?;
    regs.set_rsi(rsi.wrapping_add(step as u64));
    regs.set_rdi(rdi.wrapping_add(step as u64));
    Ok(false)
}

/// Whether two host ranges of `len` bytes overlap.
fn host_ranges_overlap(a: *mut u8, b: *mut u8, len: usize) -> bool {
    if len == 0 || a.is_null() || b.is_null() {
        return false;
    }
    // `addr()` is the provenance-preserving integer address (usize).
    let a_u = a.addr();
    let b_u = b.addr();
    let end_a = a_u.saturating_add(len.saturating_sub(1));
    let end_b = b_u.saturating_add(len.saturating_sub(1));
    a_u <= end_b && b_u <= end_a
}

fn ranges_overlap(a: u64, b: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let a_end = a.wrapping_add(len.wrapping_sub(1));
    let b_end = b.wrapping_add(len.wrapping_sub(1));
    a <= b_end && b <= a_end
}

fn exec_lods(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    size: usize,
) -> Result<(), StepExecError> {
    let stay = string_lods(mem, regs, size, has_any_rep(instr))?;
    apply_string_stay(regs, instr, stay);
    Ok(())
}

fn string_lods(
    mem: &GuestMemory,
    regs: &mut RegFile,
    size: usize,
    rep: bool,
) -> Result<bool, StepExecError> {
    if rep && regs.rcx() == 0 {
        return Ok(false);
    }
    let step = df_step(regs, size);
    if rep {
        let mut count = regs.rcx().min(REP_BULK_MAX);
        let mut rsi = regs.rsi();
        while count > 0 {
            let last = read_mem_value(mem, rsi, size)?;
            rsi = rsi.wrapping_add(step as u64);
            count = count.saturating_sub(1);
            regs.set_rsi(rsi);
            regs.set_rcx(regs.rcx().saturating_sub(1));
            match size {
                1 => regs.write_reg(Register::AL, last)?,
                4 => regs.write_reg(Register::EAX, last)?,
                _ => regs.set_rax(last),
            }
        }
        return Ok(true);
    }
    let rsi = regs.rsi();
    let v = read_mem_value(mem, rsi, size)?;
    match size {
        1 => regs.write_reg(Register::AL, v)?,
        4 => regs.write_reg(Register::EAX, v)?,
        _ => regs.set_rax(v),
    }
    regs.set_rsi(rsi.wrapping_add(step as u64));
    Ok(false)
}

fn exec_scas(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    size: usize,
) -> Result<(), StepExecError> {
    let stay = string_scas(
        mem,
        regs,
        size,
        has_any_rep(instr),
        instr.has_repe_prefix(),
        instr.has_repne_prefix(),
    )?;
    apply_string_stay(regs, instr, stay);
    Ok(())
}

fn string_scas(
    mem: &GuestMemory,
    regs: &mut RegFile,
    size: usize,
    rep: bool,
    repe: bool,
    repne: bool,
) -> Result<bool, StepExecError> {
    if rep && regs.rcx() == 0 {
        return Ok(false);
    }
    let step = df_step(regs, size);
    let acc = regs.rax() & regs::size_mask(size);
    if rep {
        let mut count = regs.rcx().min(REP_BULK_MAX);
        let mut rdi = regs.rdi();
        let mut zf_stop = false;
        while count > 0 {
            let v = read_mem_value(mem, rdi, size)? & regs::size_mask(size);
            let result = acc.wrapping_sub(v);
            regs::set_sub_flags(regs, acc, v, result, size);
            rdi = rdi.wrapping_add(step as u64);
            count = count.saturating_sub(1);
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(1));
            let zf = regs.flag(rflags::ZF);
            zf_stop = (repe && !zf) || (repne && zf);
            if zf_stop {
                break;
            }
        }
        // ZF early-exit advances RIP; RCX exhaust stays for a follow-up no-op step.
        return Ok(!zf_stop);
    }
    let rdi = regs.rdi();
    let v = read_mem_value(mem, rdi, size)? & regs::size_mask(size);
    let result = acc.wrapping_sub(v);
    regs::set_sub_flags(regs, acc, v, result, size);
    regs.set_rdi(rdi.wrapping_add(step as u64));
    Ok(false)
}

fn exec_cmps(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    size: usize,
) -> Result<(), StepExecError> {
    let stay = string_cmps(
        mem,
        regs,
        size,
        has_any_rep(instr),
        instr.has_repe_prefix(),
        instr.has_repne_prefix(),
    )?;
    apply_string_stay(regs, instr, stay);
    Ok(())
}

fn string_cmps(
    mem: &GuestMemory,
    regs: &mut RegFile,
    size: usize,
    rep: bool,
    repe: bool,
    repne: bool,
) -> Result<bool, StepExecError> {
    if rep && regs.rcx() == 0 {
        return Ok(false);
    }
    let step = df_step(regs, size);
    if rep {
        let mut count = regs.rcx().min(REP_BULK_MAX);
        let mut rsi = regs.rsi();
        let mut rdi = regs.rdi();
        let mut zf_stop = false;
        while count > 0 {
            let a = read_mem_value(mem, rsi, size)? & regs::size_mask(size);
            let b = read_mem_value(mem, rdi, size)? & regs::size_mask(size);
            let result = a.wrapping_sub(b);
            regs::set_sub_flags(regs, a, b, result, size);
            rsi = rsi.wrapping_add(step as u64);
            rdi = rdi.wrapping_add(step as u64);
            count = count.saturating_sub(1);
            regs.set_rsi(rsi);
            regs.set_rdi(rdi);
            regs.set_rcx(regs.rcx().saturating_sub(1));
            let zf = regs.flag(rflags::ZF);
            zf_stop = (repe && !zf) || (repne && zf);
            if zf_stop {
                break;
            }
        }
        return Ok(!zf_stop);
    }
    let rsi = regs.rsi();
    let rdi = regs.rdi();
    let a = read_mem_value(mem, rsi, size)? & regs::size_mask(size);
    let b = read_mem_value(mem, rdi, size)? & regs::size_mask(size);
    let result = a.wrapping_sub(b);
    regs::set_sub_flags(regs, a, b, result, size);
    regs.set_rsi(rsi.wrapping_add(step as u64));
    regs.set_rdi(rdi.wrapping_add(step as u64));
    Ok(false)
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
    let cf = regs.flag(rflags::CF); // INC/DEC do not modify CF
    if inc {
        regs::set_add_flags(regs, dst, src, result, size);
    } else {
        regs::set_sub_flags(regs, dst, src, result, size);
    }
    regs.set_flag(rflags::CF, cf);
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
    regs.set_flag(rflags::CF, dst != 0);
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
    // 64-bit mode: count masked with 0x3F; rotate width further reduces.
    let count_masked = count_raw & 0x3f;
    let width = u32::try_from(bits).unwrap_or(64);
    let count_mod = if width == 64 {
        count_masked
    } else if width == 0 {
        0
    } else {
        count_masked % width
    };
    if count_mod == 0 {
        return Ok(());
    }
    let count_usize = count_mod as usize;
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
    };
    regs.set_flag(rflags::CF, cf);
    // ROL/ROR do not update ZF/SF/PF; SHL/SHR/SAR do.
    if matches!(kind, ShiftKind::Shl | ShiftKind::Shr | ShiftKind::Sar) {
        regs.set_flag(rflags::ZF, result == 0);
        let sign = 1_u64 << bits.saturating_sub(1);
        regs.set_flag(rflags::SF, (result & sign) != 0);
        regs.set_flag(rflags::PF, (result as u8).count_ones().is_multiple_of(2));
    }
    if count_mod == 1 {
        let sign = 1_u64 << bits.saturating_sub(1);
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
        };
        regs.set_flag(rflags::OF, of);
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
            #[expect(unsafe_code, clippy::cast_ptr_alignment)]
            let atom = unsafe { &*(host.cast::<AtomicI32>()) };
            let add = i32::from_le_bytes((addend as u32).to_le_bytes());
            let old = atom.fetch_add(add, Ordering::SeqCst);
            Some(u64::from(old as u32))
        }
        8 if addr.is_multiple_of(8) => {
            let host = mem.host_span(addr, 8, true)?;
            #[expect(unsafe_code, clippy::cast_ptr_alignment)]
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

    if regs.flag(rflags::ZF) {
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

/// Dump iced-interpreter mnemonic counters to stderr (sorted by count).
/// Activated by `WIE_EXEC_TRACE=1`. Call after a guest session ends.
pub fn dump_iced_counters() {
    if !*ICED_TRACE_ENABLED {
        return;
    }
    let mut pairs: Vec<(u64, usize)> = Vec::new();
    for (i, c) in ICED_COUNTERS.iter().enumerate() {
        let count = c.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        pairs.push((count, i));
    }
    pairs.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total: u64 = pairs.iter().map(|(c, _)| *c).sum();
    eprintln!("--- iced-interp mnemonic counts (total={total}) ---");
    let show = pairs.len().min(60);
    for (count, idx) in pairs.iter().take(show) {
        let name = Mnemonic::try_from(*idx)
            .map_or_else(|_| format!("Mnemonic({idx})"), |m| format!("{m:?}"));
        // Integer tenths of a percent (avoid f64 cast_precision_loss).
        let pct = count.saturating_mul(1000).checked_div(total).unwrap_or(0);
        let pct_whole = pct / 10;
        let pct_frac = pct % 10;
        eprintln!("{count:>10}  {pct_whole:3}.{pct_frac}%  {name}");
    }
    if pairs.len() > show {
        eprintln!("  … {} more mnemonics", pairs.len() - show);
    }
    eprintln!("--- end ---");
}

/// Zero counters (for multi-run tooling). No-op when trace is disabled.
pub fn reset_iced_counters() {
    if !*ICED_TRACE_ENABLED {
        return;
    }
    for c in ICED_COUNTERS.iter() {
        c.store(0, Ordering::Relaxed);
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

fn effective_address(regs: &RegFile, instr: &Instruction) -> Result<u64, StepExecError> {
    // iced stores the absolute address for RIP/EIP-relative in memory_displacement64().
    // For other bases, displacement is a signed offset added to base+index*scale.
    let base = instr.memory_base();
    if base == Register::RIP || base == Register::EIP {
        return Ok(instr.memory_displacement64());
    }

    let mut addr = instr.memory_displacement64();

    // FS/GS segment overrides (x64: FS and GS are the only meaningful segments).
    // Windows x64 uses GS:0 as TEB base.  When an instruction carries a GS segment
    // prefix, the effective address is relative to the TEB, not to address zero.
    let seg = instr.memory_segment();
    if seg == Register::GS || seg == Register::FS {
        addr = addr.wrapping_add(crate::GS_BASE);
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

fn read_mem_value(mem: &GuestMemory, addr: u64, size: usize) -> Result<u64, StepExecError> {
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
            access_type: ACCESS_READ,
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

fn write_mem_value(
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
            access_type: ACCESS_WRITE,
            address: addr,
            size: i32::try_from(size).unwrap_or(0),
            value: i64::try_from(value).unwrap_or(0),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod string_abi_tests {
    use super::{RepPrefix, StringOpKind};

    const ALL_KINDS: [StringOpKind; 5] = [
        StringOpKind::Stos,
        StringOpKind::Movs,
        StringOpKind::Lods,
        StringOpKind::Scas,
        StringOpKind::Cmps,
    ];

    /// The JIT encodes the kind into an `extern "C"` u64 and the host helper
    /// decodes it. Encode/decode must be exact inverses or a compiled block
    /// silently performs the wrong string operation.
    #[test]
    fn string_op_kind_abi_roundtrips() {
        for kind in ALL_KINDS {
            let decoded = StringOpKind::try_from(kind.to_abi());
            assert_eq!(decoded, Ok(kind), "round-trip failed for {kind:?}");
        }
    }

    /// Discriminants are a wire format; pin them so a reordering of the enum
    /// cannot silently repoint an already-compiled encoding.
    #[test]
    fn string_op_kind_abi_values_are_stable() {
        assert_eq!(StringOpKind::Stos.to_abi(), 0);
        assert_eq!(StringOpKind::Movs.to_abi(), 1);
        assert_eq!(StringOpKind::Lods.to_abi(), 2);
        assert_eq!(StringOpKind::Scas.to_abi(), 3);
        assert_eq!(StringOpKind::Cmps.to_abi(), 4);
    }

    #[test]
    fn string_op_kind_rejects_out_of_range() {
        for raw in [5_u64, 6, u64::MAX] {
            assert_eq!(StringOpKind::try_from(raw), Err(()), "raw={raw}");
        }
    }

    /// `rep`/`repe`/`repne` occupy bits 0/1/2. The helper decodes what the
    /// lowering encoded, so every combination must survive the trip.
    #[test]
    fn rep_prefix_abi_roundtrips() {
        for bits in 0..8_u64 {
            let prefix = RepPrefix::from_abi(bits);
            assert_eq!(prefix.to_abi(), bits, "bits={bits}");
        }
        for rep in [false, true] {
            for repe in [false, true] {
                for repne in [false, true] {
                    let p = RepPrefix { rep, repe, repne };
                    assert_eq!(RepPrefix::from_abi(p.to_abi()), p);
                }
            }
        }
    }
}
