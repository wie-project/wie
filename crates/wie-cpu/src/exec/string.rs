//! REP-prefixed string operations for the iced interpreter and the JIT host
//! bridge (`wie_jit_string`).

use crate::mem::GuestMemory;
use crate::regs::{self, RegFile, Rflags};
use iced_x86::{Instruction, Mnemonic, Register};

use super::{StepExecError, read_mem_value, write_mem_value};

/// REP / REPE / REPNE present (F2/F3 string prefixes).
fn has_any_rep(instr: &Instruction) -> bool {
    instr.has_rep_prefix() || instr.has_repe_prefix() || instr.has_repne_prefix()
}

fn df_step(regs: &RegFile, size: usize) -> i64 {
    let s = i64::try_from(size).unwrap_or(1);
    if regs.flag(Rflags::DF) { -s } else { s }
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

/// Minimum byte length for host-span `memcpy`/`memset`.
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

pub(super) fn exec_stos(
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
        // Soft-translated host span → memset-like fill (DF=0 or DF=1).
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

pub(super) fn exec_movs(
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
        // Non-overlapping guest ranges + soft-translated host spans
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

pub(super) fn exec_lods(
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

pub(super) fn exec_scas(
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
            let zf = regs.flag(Rflags::ZF);
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

pub(super) fn exec_cmps(
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
            let zf = regs.flag(Rflags::ZF);
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
