//! x86-64 GPRs + RFLAGS for the iced interpreter.

use crate::CpuError;
use crate::consts::{BITS_PER_BYTE, BYTE_MASK};
use iced_x86::Register;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

/// RFLAGS bit masks (subset used by the interpreter / JIT).
///
/// A transparent newtype over the raw u64 RFLAGS word: flag bits are only
/// referenced through the named associated constants, so a typo'd bit can
/// never silently alias another flag. The inner word keeps the exact numeric
/// encoding (0/1/…/16) the memory-access layer and `InvalidMemoryAccess`
/// report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct Rflags(u64);

impl Rflags {
    /// Carry flag.
    pub const CF: Self = Self(1);
    /// Parity flag.
    pub const PF: Self = Self(1 << 2);
    /// Auxiliary carry flag.
    pub const AF: Self = Self(1 << 4);
    /// Zero flag.
    pub const ZF: Self = Self(1 << 6);
    /// Sign flag.
    pub const SF: Self = Self(1 << 7);
    /// Interrupt flag.
    pub const IF: Self = Self(1 << 9);
    /// Direction flag.
    pub const DF: Self = Self(1 << 10);
    /// Overflow flag.
    pub const OF: Self = Self(1 << 11);
    /// Architectural reserved bit 1 is always 1.
    pub const ALWAYS1: Self = Self(1 << 1);
    /// Default after reset / process start (IF + reserved bit 1).
    pub const DEFAULT: Self = Self(Self::ALWAYS1.0 | Self::IF.0);
}

impl From<u64> for Rflags {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Rflags> for u64 {
    fn from(value: Rflags) -> Self {
        value.0
    }
}

impl BitOr for Rflags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Rflags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Rflags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for Rflags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for Rflags {
    type Output = Self;
    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

/// Portable snapshot of architectural CPU state for guest thread switch.
///
/// Used when multiple host threads serialize on one shared [`crate::CpuEngine`]:
/// each guest thread parks its regs here while another runs.
#[derive(Debug, Clone)]
pub struct ThreadContext {
    /// RAX..R15.
    pub gpr: [u64; 16],
    /// XMM0..XMM15.
    pub xmm: [u128; 16],
    /// Instruction pointer.
    pub rip: u64,
    /// RFLAGS (includes reserved bit 1).
    pub rflags: Rflags,
    /// MXCSR control/status register (x86 reset value 0x1F80: all exception
    /// masks set, round-to-nearest, no DAZ/FZ).
    pub mxcsr: u32,
    /// Guest GS segment base — the TEB page this thread's GS-relative
    /// accesses resolve to. Carried through thread switches with the rest of
    /// the architectural state.
    pub gs_base: u64,
}

impl Default for ThreadContext {
    fn default() -> Self {
        Self {
            gpr: [0; 16],
            xmm: [0; 16],
            rip: 0,
            rflags: Rflags::DEFAULT,
            mxcsr: RegFile::MXCSR_DEFAULT,
            gs_base: crate::GS_BASE,
        }
    }
}

impl ThreadContext {
    /// Fresh context with default RFLAGS (IF + reserved bit 1).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// General-purpose register file + XMM + RIP + RFLAGS (64-bit mode only).
#[derive(Debug, Clone)]
pub struct RegFile {
    /// RAX..R15 (index = `Register::RAX.number()` …).
    gpr: [u64; 16],
    /// XMM0..XMM15 as 128-bit values (low 64 used by scalar SSE2).
    xmm: [u128; 16],
    pub rip: u64,
    pub rflags: Rflags,
    /// MXCSR control/status register (x86 reset value 0x1F80: all exception
    /// masks set, round-to-nearest, no DAZ/FZ). FP ops use the native ARM64
    /// rounding, so only the stored value is tracked (matches the default
    /// round-to-nearest behavior guests rely on).
    pub mxcsr: u32,
    /// Guest GS segment base — the TEB page this engine's GS-relative
    /// accesses resolve to. The primary thread keeps the fixed [`crate::GS_BASE`];
    /// workers are rebound to their per-thread TEB page.
    gs_base: u64,
    /// x87 register stack, PHYSICAL indices st(0)=stack top: physical slot
    /// `x87_top` holds st(0), `(x87_top + i) % 8` holds st(i). The JIT never
    /// touches these (x87 lowers to the interpreter, which now degrades
    /// instead of stopping on the exotic remainder).
    pub(crate) x87: [f64; 8],
    /// x87 TOP-of-stack pointer — the PHYSICAL index of st(0) (0..7).
    pub(crate) x87_top: u8,
    /// x87 status word: condition codes (C0/C1/C2/C3) + TOP + exception
    /// bits the guest reads after `fcom` / `fnstsw`.
    pub(crate) x87_sw: u16,
    /// x87 control word (stored; only used by guests that flip precision).
    pub(crate) x87_cw: u16,
}

impl Default for RegFile {
    fn default() -> Self {
        Self {
            gpr: [0; 16],
            xmm: [0; 16],
            rip: 0,
            rflags: Rflags::DEFAULT,
            mxcsr: Self::MXCSR_DEFAULT,
            gs_base: crate::GS_BASE,
            x87: [0.0; 8],
            x87_top: 0,
            x87_sw: 0,
            x87_cw: 0x037F,
        }
    }
}

/// x87 status-word condition-code bits (`fnstsw` / `fstsw` observers).
pub(crate) const X87_C0: u16 = 1 << 8;
#[allow(dead_code)] // used when the C1 flag gains a consumer (fprem sign etc.)
pub(crate) const X87_C1: u16 = 1 << 9;
pub(crate) const X87_C2: u16 = 1 << 10;
pub(crate) const X87_C3: u16 = 1 << 14;

impl RegFile {
    /// MXCSR exception-mask field: bits 7–12, one per masked exception
    /// (invalid-op, denormal, zero-divide, overflow, underflow, precision).
    pub const MXCSR_EXC_MASKS: u32 = 0x3f << 7;

    /// x86 MXCSR reset value: all six exception masks set (`0x1F80`), rounding
    /// control = nearest (field 0), DAZ/FZ clear.
    pub const MXCSR_DEFAULT: u32 = Self::MXCSR_EXC_MASKS;

    /// Create a fresh all-zero register file with default RFLAGS.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Export a full architectural snapshot for a guest thread switch.
    #[must_use]
    pub fn snapshot(&self) -> ThreadContext {
        ThreadContext {
            gpr: self.gpr,
            xmm: self.xmm,
            rip: self.rip,
            rflags: self.rflags,
            mxcsr: self.mxcsr,
            gs_base: self.gs_base,
        }
    }

    /// Restore a full architectural snapshot after a guest thread switch.
    pub fn restore(&mut self, ctx: &ThreadContext) {
        self.gpr = ctx.gpr;
        self.xmm = ctx.xmm;
        self.rip = ctx.rip;
        self.set_rflags_checked(ctx.rflags);
        self.mxcsr = ctx.mxcsr;
        self.gs_base = ctx.gs_base;
    }

    /// The guest GS segment base (this thread's TEB page).
    #[must_use]
    pub fn gs_base(&self) -> u64 {
        self.gs_base
    }

    /// Rebind the GS segment base to another TEB page.
    pub fn set_gs_base(&mut self, base: u64) {
        self.gs_base = base;
    }

    /// Read the guest MXCSR control/status register.
    #[must_use]
    pub fn mxcsr(&self) -> u32 {
        self.mxcsr
    }

    /// Write the guest MXCSR control/status register.
    pub(crate) fn set_mxcsr(&mut self, value: u32) {
        self.mxcsr = value;
    }

    /// Read GPR `idx` (RAX=0 … R15=15); out-of-range reads yield 0.
    #[must_use]
    pub fn gpr(&self, idx: usize) -> u64 {
        self.gpr.get(idx).copied().unwrap_or(0)
    }

    /// Write GPR `idx`; out-of-range writes are ignored.
    pub(crate) fn set_gpr(&mut self, idx: usize, value: u64) {
        if let Some(slot) = self.gpr.get_mut(idx) {
            *slot = value;
        }
    }

    /// Public GPR write for guest thread bootstrap (thread start RCX/RSP/…).
    pub fn set_gpr_public(&mut self, idx: usize, value: u64) {
        self.set_gpr(idx, value);
    }

    /// Read RAX.
    #[must_use]
    pub fn rax(&self) -> u64 {
        self.gpr(0)
    }
    /// Write RAX.
    pub(crate) fn set_rax(&mut self, v: u64) {
        self.set_gpr(0, v);
    }
    /// Read RCX.
    #[must_use]
    pub fn rcx(&self) -> u64 {
        self.gpr(1)
    }
    /// Write RCX.
    pub(crate) fn set_rcx(&mut self, v: u64) {
        self.set_gpr(1, v);
    }
    /// Read RDX.
    #[must_use]
    pub fn rdx(&self) -> u64 {
        self.gpr(2)
    }
    /// Write RDX.
    pub(crate) fn set_rdx(&mut self, v: u64) {
        self.set_gpr(2, v);
    }
    /// Read RBX.
    #[must_use]
    pub fn rbx(&self) -> u64 {
        self.gpr(3)
    }
    /// Read RSP.
    #[must_use]
    pub fn rsp(&self) -> u64 {
        self.gpr(4)
    }
    /// Write RSP.
    pub(crate) fn set_rsp(&mut self, v: u64) {
        self.set_gpr(4, v);
    }
    /// Read RBP.
    #[must_use]
    pub fn rbp(&self) -> u64 {
        self.gpr(5)
    }
    /// Write RBP.
    pub(crate) fn set_rbp(&mut self, v: u64) {
        self.set_gpr(5, v);
    }
    /// Read RSI.
    #[must_use]
    pub fn rsi(&self) -> u64 {
        self.gpr(6)
    }
    /// Write RSI.
    pub(crate) fn set_rsi(&mut self, v: u64) {
        self.set_gpr(6, v);
    }
    /// Read RDI.
    #[must_use]
    pub fn rdi(&self) -> u64 {
        self.gpr(7)
    }
    /// Write RDI.
    pub(crate) fn set_rdi(&mut self, v: u64) {
        self.set_gpr(7, v);
    }
    /// Read R8.
    #[must_use]
    pub fn r8(&self) -> u64 {
        self.gpr(8)
    }
    /// Write R8.
    pub(crate) fn set_r8(&mut self, v: u64) {
        self.set_gpr(8, v);
    }
    /// Read R9.
    #[must_use]
    pub fn r9(&self) -> u64 {
        self.gpr(9)
    }
    /// Write R9.
    pub(crate) fn set_r9(&mut self, v: u64) {
        self.set_gpr(9, v);
    }

    /// Read a GPR / partial register (64-bit mode).
    pub fn read_reg(&self, reg: Register) -> Result<u64, CpuError> {
        // Fast paths for the two dominant operand forms in x86-64 code.
        //
        // `is_gpr64` / `is_gpr32` are inline discriminant range checks, and for
        // those ranges `number()` is exactly the GPR index (RAX→0 … R15→15,
        // EAX→0 … R15D→15). This collapses the three iced table lookups the
        // general path performs (`size()`, `full_register()`, `gpr_index()`)
        // into one, and skips the `Register::None` / RIP / AH-BH tests.
        if reg.is_gpr64() {
            return Ok(self.gpr(reg.number()));
        }
        if reg.is_gpr32() {
            // 32-bit read yields the zero-extended low dword.
            return Ok(self.gpr(reg.number()) & 0xffff_ffff);
        }
        if reg == Register::None {
            return Ok(0);
        }
        if reg == Register::RIP {
            return Ok(self.rip);
        }
        if matches!(
            reg,
            Register::AH | Register::CH | Register::DH | Register::BH
        ) {
            let full = reg.full_register();
            let idx = gpr_index(full)?;
            let full_val = self.gpr(idx);
            return Ok((full_val >> 8) & 0xff);
        }
        let size = reg.size();
        let full = reg.full_register();
        let idx = gpr_index(full)?;
        let full_val = self.gpr(idx);
        Ok(match size {
            1 => full_val & 0xff,
            2 => full_val & 0xffff,
            4 => full_val & 0xffff_ffff,
            8 => full_val,
            _ => {
                return Err(CpuError::Message(format!(
                    "unsupported register size {size} for {reg:?}"
                )));
            }
        })
    }

    /// Write a GPR / partial register. 32-bit writes zero-extend the full 64-bit register.
    pub fn write_reg(&mut self, reg: Register, value: u64) -> Result<(), CpuError> {
        // Fast paths mirroring `read_reg` — see the rationale there.
        if reg.is_gpr64() {
            self.set_gpr(reg.number(), value);
            return Ok(());
        }
        if reg.is_gpr32() {
            // x86-64: a 32-bit write zero-extends into the full 64-bit register.
            self.set_gpr(reg.number(), value & 0xffff_ffff);
            return Ok(());
        }
        if reg == Register::None {
            return Ok(());
        }
        if reg == Register::RIP {
            self.rip = value;
            return Ok(());
        }
        if matches!(
            reg,
            Register::AH | Register::CH | Register::DH | Register::BH
        ) {
            let full = reg.full_register();
            let idx = gpr_index(full)?;
            let old = self.gpr(idx);
            let new = (old & !0xff00) | ((value & 0xff) << 8);
            self.set_gpr(idx, new);
            return Ok(());
        }
        let size = reg.size();
        let full = reg.full_register();
        let idx = gpr_index(full)?;
        let old = self.gpr(idx);
        let new = match size {
            1 => (old & !0xff) | (value & 0xff),
            2 => (old & !0xffff) | (value & 0xffff),
            4 => value & 0xffff_ffff, // zero-extend to 64
            8 => value,
            _ => {
                return Err(CpuError::Message(format!(
                    "unsupported register size {size} for {reg:?}"
                )));
            }
        };
        self.set_gpr(idx, new);
        Ok(())
    }

    /// Test whether any bit of `mask` is set in RFLAGS.
    #[must_use]
    pub fn flag(&self, mask: Rflags) -> bool {
        u64::from(self.rflags & mask) != 0
    }

    /// Read XMM0–XMM15 (128-bit).
    pub fn read_xmm(&self, reg: Register) -> Result<u128, CpuError> {
        if !reg.is_xmm() {
            return Err(CpuError::Message(format!("not an XMM register: {reg:?}")));
        }
        let n = reg.number();
        self.xmm
            .get(n)
            .copied()
            .ok_or_else(|| CpuError::Message(format!("XMM index {n} OOB")))
    }

    /// Write XMM0–XMM15 (128-bit).
    pub fn write_xmm(&mut self, reg: Register, value: u128) -> Result<(), CpuError> {
        if !reg.is_xmm() {
            return Err(CpuError::Message(format!("not an XMM register: {reg:?}")));
        }
        let n = reg.number();
        let slot = self
            .xmm
            .get_mut(n)
            .ok_or_else(|| CpuError::Message(format!("XMM index {n} OOB")))?;
        *slot = value;
        Ok(())
    }

    /// Read XMM by index 0..15 (JIT snapshot).
    #[must_use]
    pub fn xmm_at(&self, idx: usize) -> u128 {
        self.xmm.get(idx).copied().unwrap_or(0)
    }

    /// Write XMM by index 0..15 (JIT write-back).
    pub fn set_xmm_at(&mut self, idx: usize, value: u128) {
        if let Some(slot) = self.xmm.get_mut(idx) {
            *slot = value;
        }
    }

    /// Set or clear the flags in `mask`, branchlessly.
    ///
    /// `on` selects between the mask and zero arithmetically: `0 - on` wraps to
    /// all-ones when `on` is true and 0 when false, so `mask & (0 - on)` is
    /// `mask` or `0`. The per-flag `if` would otherwise be a data-dependent
    /// branch executed ~6 times per arithmetic op in the interpreter.
    /// (No `|= ALWAYS1` here: bit 1 does not overlap any flag mask defined on
    /// `Rflags`, so a per-flag re-assert is pure overhead; the invariant is
    /// instead established at every wholesale RFLAGS assignment via
    /// [`Self::set_rflags_checked`].)
    pub(crate) fn set_flag(&mut self, mask: Rflags, on: bool) {
        let m = u64::from(mask);
        let sel = m & 0_u64.wrapping_sub(u64::from(on));
        self.rflags = Rflags::from((u64::from(self.rflags) & !m) | sel);
    }

    /// Assign the whole RFLAGS word, re-asserting the architectural reserved
    /// bit 1. This is the single place the `ALWAYS1` invariant is maintained;
    /// use it for any bulk assignment (thread-context restore, JIT writeback).
    pub(crate) fn set_rflags_checked(&mut self, value: Rflags) {
        self.rflags = Rflags::from(u64::from(value) | u64::from(Rflags::ALWAYS1));
    }
}

fn gpr_index(full: Register) -> Result<usize, CpuError> {
    // RAX..R15 map to numbers 0..15.
    let n = full.number();
    if n < 16 && full.size() == 8 {
        Ok(n)
    } else {
        Err(CpuError::Message(format!(
            "not a 64-bit GPR: {full:?} (number={n})"
        )))
    }
}

/// Bit width of an operand of `size` bytes (`BITS_PER_BYTE * size`).
#[must_use]
pub(crate) fn size_bits(size: usize) -> u32 {
    u32::try_from(size)
        .unwrap_or(0)
        .saturating_mul(BITS_PER_BYTE)
}

/// Sign bit of an operand of `size` bytes (`1 << (bits - 1)`).
#[must_use]
pub(crate) fn size_sign_bit(size: usize) -> u64 {
    1_u64 << size_bits(size).saturating_sub(1)
}

/// Update ZF/SF/PF from a result of `size` bytes; leave CF/OF/AF to caller.
pub(crate) fn set_logic_flags(regs: &mut RegFile, result: u64, size: usize) {
    let mask = size_mask(size);
    let v = result & mask;
    regs.set_flag(Rflags::ZF, v == 0);
    let sign_bit = size_sign_bit(size);
    regs.set_flag(Rflags::SF, (v & sign_bit) != 0);
    regs.set_flag(Rflags::PF, parity_even(low_byte(v)));
    regs.set_flag(Rflags::CF, false);
    regs.set_flag(Rflags::OF, false);
    // AF undefined for logic; leave unchanged.
}

/// Shared flag-update core for ADD/SUB (and CMP, which is SUB for flags).
/// `d`/`s`/`r` are the caller-masked operands and result; only CF/OF differ
/// between the two modes, so the caller computes those and passes them in.
pub(crate) fn set_arith_flags(
    regs: &mut RegFile,
    d: u64,
    s: u64,
    r: u64,
    size: usize,
    cf: bool,
    of: bool,
) {
    let sign = size_sign_bit(size);

    regs.set_flag(Rflags::CF, cf);
    regs.set_flag(Rflags::ZF, r == 0);
    regs.set_flag(Rflags::SF, (r & sign) != 0);
    regs.set_flag(Rflags::PF, parity_even(low_byte(r)));
    regs.set_flag(Rflags::OF, of);
    regs.set_flag(Rflags::AF, ((d ^ s ^ r) & 0x10) != 0);
}

/// Update flags after ADD.
pub(crate) fn set_add_flags(regs: &mut RegFile, dst: u64, src: u64, result: u64, size: usize) {
    let mask = size_mask(size);
    let d = dst & mask;
    let s = src & mask;
    let r = result & mask;
    let sign = size_sign_bit(size);

    let wide = u128::from(d).wrapping_add(u128::from(s));
    // OF: same sign operands, result different sign
    let of = ((d ^ r) & (s ^ r) & sign) != 0;
    set_arith_flags(regs, d, s, r, size, wide > u128::from(mask), of);
}

/// Update flags after SUB / CMP.
pub(crate) fn set_sub_flags(regs: &mut RegFile, dst: u64, src: u64, result: u64, size: usize) {
    let mask = size_mask(size);
    let d = dst & mask;
    let s = src & mask;
    let r = result & mask;
    let sign = size_sign_bit(size);

    // OF: different sign operands, result sign != dst sign
    let of = ((d ^ s) & (d ^ r) & sign) != 0;
    set_arith_flags(regs, d, s, r, size, d < s, of);
}

#[must_use]
pub(crate) fn size_mask(size: usize) -> u64 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

/// Low 8 bits of `v` without `as` casts (always fits in `u8`).
#[inline]
#[must_use]
fn low_byte(v: u64) -> u8 {
    u8::try_from(v & BYTE_MASK).unwrap_or(0)
}

#[must_use]
fn parity_even(byte: u8) -> bool {
    byte.count_ones().is_multiple_of(2)
}
