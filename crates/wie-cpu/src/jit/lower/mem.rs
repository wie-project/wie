//! Guest-memory access: host-callable `wie_jit_*` helpers (registered as JIT
//! symbols) and the IR-emitting load/store/pin helpers used by block emission.

use super::super::block::DecodedInsn;
use super::emit::{HoistedPin, MemEnv, RefillProbe, check_fault_after_ucrt};
use super::err::LowerError;
use super::flags::iconst_u64;
use super::tlb::{set_fault, tlb_page_ptr};
use super::{
    CHAIN_SLOTS, EDGE_IC_SLOTS, GOLDEN_RATIO_64, JitCtx, OFF_MEM_GEN, OFF_PINS, OFF_REFILL,
    PIN_STRIDE, REFILL_OFFSET_MASK, REFILL_SHIFT, REFILL_STRIDE_SHIFT, TLB_PROT_R, TLB_PROT_W,
    chain_hash,
};

use crate::exec::{self, StringOpKind};
use crate::jit::config::JitConfig;
use crate::mem::PAGE_SIZE;
use crate::regs::{RegFile, Rflags};
use cranelift::codegen::ir::{BlockArg, FuncRef};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;

// --- Host mem helpers (registered as JIT symbols) ---

/// Soft-translate a contiguous guest span to a host pointer (0 on failure).
///
/// Used by inline string fast-path; never returns a guest VA.
pub(crate) unsafe extern "C" fn wie_jit_host_span(
    ctx: *mut JitCtx,
    guest_va: u64,
    len: u64,
    write: u64,
) -> u64 {
    if ctx.is_null() || len == 0 {
        return 0;
    }
    // SAFETY: live JitCtx for the block.
    let ctx = unsafe { &mut *ctx };
    if ctx.fault != 0 {
        return 0;
    }
    let len_usize = usize::try_from(len).unwrap_or(0);
    if len_usize == 0 {
        return 0;
    }
    // SAFETY: mem pointer set by run_compiled.
    let mem = unsafe { &*ctx.mem };
    match mem.host_span(guest_va, len_usize, write != 0) {
        Some(p) if !p.is_null() => p as u64,
        _ => 0,
    }
}

/// Last-resort successor resolution for a chain edge (0 = miss). Used for
/// late-bound chaining, i.e. **after** the emitted edge has failed to resolve
/// the target from its edge-IC branch cascade and from the chain table's first
/// two probe slots (both probed inline by `emit_chain_or_exit`).
///
/// Checks monomorphic edge IC first, then the open-addressing chain table,
/// walking the rest of the linear probe. On a table hit, installs into an
/// edge-IC slot for the next transfer. Every entry is charged to
/// [`JitCtx::chain_helper_calls`], the counter that measures how much of the
/// chain traffic still leaves emitted code.
///
/// `extern "C" fn(ctx, va) -> fn_ptr`
pub(crate) unsafe extern "C" fn wie_jit_chain_lookup(ctx: *mut JitCtx, va: u64) -> u64 {
    if va == 0 || ctx.is_null() {
        return 0;
    }
    // SAFETY: `run_compiled` sets chain_* to live tables for the block duration.
    let ctx = unsafe { &mut *ctx };
    ctx.chain_helper_calls = ctx.chain_helper_calls.saturating_add(1);
    // Edge IC (data plane): monomorphic last-hit successors.
    for i in 0..EDGE_IC_SLOTS {
        if ctx.edge_ic_va[i] == va {
            let f = ctx.edge_ic_fn[i];
            if f != 0 {
                return f;
            }
        }
    }
    if ctx.chain_slots.is_null() {
        return 0;
    }
    // SAFETY: `chain_slots` points to a live [ChainSlot; CHAIN_SLOTS] array for
    // the duration of this call (set by `run_compiled`).
    let slots = unsafe { std::slice::from_raw_parts(ctx.chain_slots, CHAIN_SLOTS) };
    let mut i = chain_hash(va);
    // Bounded probe; empty slot ends search.
    for _ in 0..16 {
        let s = slots[i];
        if s.va == va {
            if s.fn_ptr != 0 {
                // Install monomorphic edge IC (RR victim).
                let slot =
                    usize::try_from(ctx.edge_ic_rr % u64::try_from(EDGE_IC_SLOTS).unwrap_or(4))
                        .unwrap_or(0);
                ctx.edge_ic_va[slot] = va;
                ctx.edge_ic_fn[slot] = s.fn_ptr;
                ctx.edge_ic_rr = ctx.edge_ic_rr.wrapping_add(1);
            }
            return s.fn_ptr;
        }
        if s.va == 0 {
            return 0;
        }
        i = (i + 1) & (CHAIN_SLOTS - 1);
    }
    0
}

/// `WIE_MT_DEBUG=1` prints the exact reason a JIT load/store stub faults.
///
/// The emitted fast path (`tlb_page_ptr`) cannot fault; every stub fault is the
/// slow path's `GuestMemory` access failing, and until this line existed the
/// reason was discarded — which is why two rounds of attribution could not tell a
/// generation race from a permission denial from an unmapped page.
static STUB_DEBUG: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("WIE_MT_DEBUG").is_ok_and(|v| v == "1"));

/// `extern "C" fn(ctx, addr, size, insn_ip) -> value`
pub(crate) unsafe extern "C" fn wie_jit_load(
    ctx: *mut JitCtx,
    addr: u64,
    size: u64,
    insn_ip: u64,
) -> u64 {
    // SAFETY: caller passes a live `JitCtx` for the duration of the block.
    let ctx = unsafe { &mut *ctx };
    ctx.load_calls = ctx.load_calls.saturating_add(1);

    if ctx.fault != 0 {
        return 0;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if size_usize == 0 || size_usize > 8 {
        set_fault(ctx, insn_ip, addr, size, 0);
        return 0;
    }
    // Fast path: single-page TLB with SPC R bit + generation.
    // SAFETY: TLB pointer is a live page from guest map for this block.
    if let Some(p) = unsafe { tlb_page_ptr(ctx, addr, size_usize, false) } {
        let mut buf = [0_u8; 8];
        // SAFETY: `p` points into a mapped page with `size_usize` bytes in range.
        unsafe {
            std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), size_usize);
        }
        return match size_usize {
            1 => u64::from(buf[0]),
            2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])),
            4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
            8 => u64::from_le_bytes(buf),
            _ => 0,
        };
    }
    // Slow path: multi-page, miss, or SPC deny on TLB.
    // SAFETY: `mem` set by `run_compiled`.
    let mem = unsafe { &*ctx.mem };
    let mut buf = [0_u8; 8];
    if let Err(e) = mem.read(addr, &mut buf[..size_usize]) {
        if *STUB_DEBUG {
            eprintln!(
                "[stub] read addr={addr:#x} size={size} rip={insn_ip:#x} access=0 fault: {e}"
            );
        }
        set_fault(ctx, insn_ip, addr, size, 0);
        return 0;
    }
    match size_usize {
        1 => u64::from(buf[0]),
        2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])),
        4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
        8 => u64::from_le_bytes(buf),
        _ => 0,
    }
}

/// `extern "C" fn(ctx, addr, size, value, insn_ip)`
pub(crate) unsafe extern "C" fn wie_jit_store(
    ctx: *mut JitCtx,
    addr: u64,
    size: u64,
    value: u64,
    insn_ip: u64,
) {
    // SAFETY: caller passes a live `JitCtx` for the duration of the block.
    let ctx = unsafe { &mut *ctx };
    ctx.store_calls = ctx.store_calls.saturating_add(1);
    if ctx.fault != 0 {
        return;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if size_usize == 0 || size_usize > 8 {
        set_fault(ctx, insn_ip, addr, size, 1);
        return;
    }
    let bytes = value.to_le_bytes();
    // SAFETY: TLB pointer is a live page from guest map; W bit checked.
    if let Some(p) = unsafe { tlb_page_ptr(ctx, addr, size_usize, true) } {
        // SAFETY: `p` points into a mapped page with `size_usize` bytes in range.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, size_usize);
        }
        return;
    }
    // SAFETY: `mem` set by `run_compiled`.
    let mem = unsafe { &*ctx.mem };
    if let Err(e) = mem.write(addr, &bytes[..size_usize]) {
        if *STUB_DEBUG {
            eprintln!(
                "[stub] write addr={addr:#x} size={size} rip={insn_ip:#x} access=1 fault: {e}"
            );
        }
        set_fault(ctx, insn_ip, addr, size, 1);
    }
}

/// Bulk string helper: `(ctx, op, size, flags, insn_ip) -> stay`.
///
/// `op`: 0=stos 1=movs 2=lods 3=scas 4=cmps
/// `flags`: bit0=rep, bit1=repe, bit2=repne
/// Returns 1 if RIP should stay on `insn_ip`, 0 to fall through (caller uses next_ip).
pub(crate) unsafe extern "C" fn wie_jit_string(
    ctx: *mut JitCtx,
    op: u64,
    size: u64,
    flags: u64,
    insn_ip: u64,
) -> u64 {
    // SAFETY: live JitCtx for the block.
    let ctx = unsafe { &mut *ctx };
    if ctx.fault != 0 {
        return 0;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if !matches!(size_usize, 1 | 2 | 4 | 8) {
        set_fault(ctx, insn_ip, 0, size, 0);
        return 0;
    }
    let Ok(kind) = StringOpKind::try_from(op) else {
        set_fault(ctx, insn_ip, 0, size, 0);
        return 0;
    };
    let rep = exec::RepPrefix::from_abi(flags);

    let mut regs = RegFile::new();
    for i in 0..16 {
        regs.set_gpr(i, ctx.gpr[i]);
    }
    regs.set_rflags_checked(Rflags::from(ctx.rflags));
    regs.rip = insn_ip;

    // SAFETY: mem pointer set by run_compiled.
    let mem = unsafe { &*ctx.mem };
    match exec::run_string_op(mem, &mut regs, kind, size_usize, rep) {
        Ok(stay) => {
            for i in 0..16 {
                ctx.gpr[i] = regs.gpr(i);
            }
            ctx.rflags = u64::from(regs.rflags);
            u64::from(stay)
        }
        Err(exec::StepExecError::InvalidMemory(inv)) => {
            for i in 0..16 {
                ctx.gpr[i] = regs.gpr(i);
            }
            ctx.rflags = u64::from(regs.rflags);
            set_fault(
                ctx,
                insn_ip,
                inv.address,
                u64::try_from(inv.size).unwrap_or(0),
                u64::try_from(inv.access_type.as_i32()).unwrap_or(0),
            );
            0
        }
        Err(exec::StepExecError::Cpu(_)) => {
            set_fault(ctx, insn_ip, 0, size, 0);
            0
        }
    }
}

/// Load one pin slot into SSA once (block entry). Fields are invariant until
/// the next `run_compiled` (protect/free bumps gen and refills pins).
pub(super) fn hoist_pin_slot(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    slot: usize,
) -> HoistedPin {
    let base_off = i64::from(OFF_PINS) + i64::from(PIN_STRIDE) * i64::try_from(slot).unwrap_or(0);
    let gb_p = bcx.ins().iadd_imm(ctx_ptr, base_off);
    let ge_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 8);
    let hb_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 16);
    let gen_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 24);
    let allow_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 32);
    let mem_gen_p = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_MEM_GEN));

    let guest_base = bcx.ins().load(types::I64, flags, gb_p, 0);
    let guest_end = bcx.ins().load(types::I64, flags, ge_p, 0);
    let host_base = bcx.ins().load(types::I64, flags, hb_p, 0);
    let pin_gen = bcx.ins().load(types::I64, flags, gen_p, 0);
    let allow = bcx.ins().load(types::I64, flags, allow_p, 0);
    let mem_gen = bcx.ins().load(types::I64, flags, mem_gen_p, 0);

    let base_nz = bcx.ins().icmp_imm(IntCC::NotEqual, host_base, 0);
    let gen_ok = bcx.ins().icmp(IntCC::Equal, pin_gen, mem_gen);
    let live = bcx.ins().band(base_nz, gen_ok);
    HoistedPin {
        guest_base,
        guest_end,
        host_base,
        allow,
        live,
    }
}

/// Emit one guard of a conjunction as a conditional branch to `fail`.
///
/// A probe's conditions are only ever consumed as "all of them hold": the
/// caller branches to its hit path when the conjunction is true and to its miss
/// path otherwise. Materialising that conjunction as an `i1` (`band` of the
/// `icmp` results, then one `brif` on the result) costs, per condition,
/// `cmp` + `cset` + `and` on aarch64, plus `tst` + `b.ne` at the end — the
/// backend at `opt_level = none` has no pass that turns those back into
/// branches. Expressing it as control flow instead costs `cmp` + `b.cc` per
/// condition: control falls through the whole chain on success and jumps
/// straight to `fail` at the first condition that does not hold, which is the
/// same predicate by construction (`A ∧ B ∧ C` is reachable-from fall-through
/// iff all three hold).
///
/// The decision is unchanged because no condition is dropped, reordered into a
/// different answer, or weakened: every `icmp` is still emitted, and only the
/// way its result is *consumed* changes.
///
/// On return the builder is positioned in the continuation block, which is
/// sealed (its only predecessor is the branch just emitted).
fn guard_exit(bcx: &mut FunctionBuilder<'_>, cond: Value, fail: Block) {
    let cont = bcx.create_block();
    bcx.ins().brif(cond, cont, &[], fail, &[]);
    bcx.switch_to_block(cont);
    bcx.seal_block(cont);
}

/// Bounds + R/W probe against a **hoisted** pin (no JitCtx reloads).
///
/// Soft translate: `host = host_base + (addr - guest_base)`.
///
/// The five conditions ([`HoistedPin::live`], no-wrap, both bounds, the R/W
/// bit) are emitted as guarded exits to `fail` — see [`guard_exit`] — and the
/// last one is returned for the caller's own branch into its hit block, so a
/// probe costs one conditional branch per condition and nothing else. `host` is
/// defined in the block that returns, so it dominates the caller's hit block.
pub(super) fn hoisted_pin_probe(
    bcx: &mut FunctionBuilder<'_>,
    pin: &HoistedPin,
    addr: Value,
    size: u32,
    write: bool,
    fail: Block,
) -> (Value, Value) {
    let size_v = iconst_u64(bcx, u64::from(size));
    let end = bcx.ins().iadd(addr, size_v);
    let no_wrap = bcx.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, end, addr);
    let lo_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, addr, pin.guest_base);
    let hi_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, end, pin.guest_end);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let need_v = iconst_u64(bcx, need);
    let prot_bits = bcx.ins().band(pin.allow, need_v);
    let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, prot_bits, 0);

    let rel = bcx.ins().isub(addr, pin.guest_base);
    let host = bcx.ins().iadd(pin.host_base, rel);

    guard_exit(bcx, pin.live, fail);
    guard_exit(bcx, no_wrap, fail);
    guard_exit(bcx, lo_ok, fail);
    guard_exit(bcx, hi_ok, fail);
    (prot_ok, host)
}

/// Hoist the two block-invariant inputs of the refill probe: the table base
/// pointer and the frame's memory generation.
///
/// Same argument as [`hoist_pin_slot`]: `mem_gen` and `refill` are invariant for
/// the whole block (a block is straight-line, and the only thing that moves the
/// generation is a guest-initiated map/protect, which returns through the
/// dispatcher before the next block), so loading them once at block entry beats
/// paying `iadd_imm` + `load` on every memory operand — and with
/// `opt_level = none` nothing downstream removes the redundancy.
pub(super) fn hoist_refill(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
) -> RefillProbe {
    let table = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_REFILL));
    let mem_gen_p = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_MEM_GEN));
    let mem_gen = bcx.ins().load(types::I64, flags, mem_gen_p, 0);
    RefillProbe {
        table,
        mem_gen,
        // The caller fills these in with `attach_homes` once it knows the block's
        // access widths; `None`/empty means the block emits today's code.
        homes: Vec::new(),
    }
}

/// Probe the direct-mapped refill table — the inline fast path for **every**
/// memory operand the emitter lowers.
///
/// Returns `(last_condition, host_ptr)`; the caller branches to its hit path on
/// `last_condition` and to its miss path otherwise. `host_ptr` is only
/// meaningful on the true edge, and is defined in the block that returns the
/// condition, so it dominates the hit block.
/// One slot, one compare, no cascade:
///
/// ```text
/// slot_off = ((page_key * GOLDEN) >> 51) & 0x1FE0     // 256 slots × 32 B
/// slot     = refill + slot_off
/// ok       = slot.tag == page_key ^ mem_gen           // key *and* generation
///          & (slot.prot & need) != 0                  // R/W bit for this access
///          & page_off + size <= PAGE_SIZE             // does not cross a page
/// host     = slot.base + page_off
/// ```
///
/// The three terms of `ok` are **anded and consumed once**, so they are emitted
/// as a chain of guarded exits ([`guard_exit`]) rather than as an `i1`
/// conjunction: one conditional branch per term. The slot arithmetic (index,
/// three slot loads, `eor`, the compares) is unchanged and stays shared.
///
/// Measured from the emitted aarch64, not from CLIF, in guest block
/// `0x1800a1c60` (SDL2 `memcpy4`, six guest instructions): this probe is
/// **28 → 23 host instructions**, and the part that exists only to combine and
/// test the three terms (`cset`/`and`/`tst`/`b.ne` → `b.hi`/`b.ne`/`cbnz`) is
/// **7 → 3**. The stack-pin probe next to it in the same block goes **21 → 17**
/// on the all-true path for the same reason. Single run; see
/// `docs/architecture/instruction-selection-audit.md` §5.1.
///
/// On a failed condition the caller falls through to the next probe stage and
/// finally the `wie_jit_load` / `wie_jit_store` helper, which re-resolves
/// through the 4-way bucket / pins / page walk and re-installs this slot. Fault
/// semantics are therefore untouched: a cross-page, denied or unmapped access
/// never takes this path, it takes the helper that reports it through
/// `set_fault`.
///
/// # Why the low bits of the VA are not the index
///
/// See [`refill_slot`](super::refill_slot): a page key's low bits are the page
/// number inside a small address window, so they partition a working set by
/// address layout instead of scattering it, and a modular multiply leaves them
/// untouched — the entropy that separates two nearby pages lives in the top
/// bits of the product, which is what the shift harvests.
pub(super) fn refill_tlb_probe(
    bcx: &mut FunctionBuilder<'_>,
    refill: &RefillProbe,
    flags: MemFlagsData,
    addr: Value,
    size: u32,
    write: bool,
    fail: Block,
) -> (Value, Value) {
    // The two halves of the VA the probe needs: page offset and page key.
    let page_off = bcx.ins().band_imm(addr, (PAGE_SIZE - 1).cast_signed());
    let page_key = bcx.ins().ushr_imm(addr, 12);
    // `page_off + size <= PAGE_SIZE` ⇒ the access cannot cross a page, which is
    // the precondition for `slot.base + page_off` to be the whole access.
    let end = bcx.ins().iadd_imm(page_off, i64::from(size));
    let in_page = bcx
        .ins()
        .icmp_imm(IntCC::UnsignedLessThanOrEqual, end, PAGE_SIZE.cast_signed());
    // Golden-ratio scatter, scaled to the slot stride by folding the multiply
    // into the shift: `((k * G) >> 56) * 32 == ((k * G) >> 51) & 0x1FE0`.
    let h = bcx.ins().imul_imm(page_key, GOLDEN_RATIO_64.cast_signed());
    let shifted = bcx
        .ins()
        .ushr_imm(h, i64::from(REFILL_SHIFT - REFILL_STRIDE_SHIFT));
    let slot_off = bcx.ins().band_imm(shifted, REFILL_OFFSET_MASK);
    let slot = bcx.ins().iadd(refill.table, slot_off);
    let tag = bcx.ins().load(types::I64, flags, slot, 0);
    let base = bcx.ins().load(types::I64, flags, slot, 8);
    let prot = bcx.ins().load(types::I64, flags, slot, 16);
    // One compare covers page identity *and* mapping generation (see `RefillSlot`).
    let want = bcx.ins().bxor(page_key, refill.mem_gen);
    let key_ok = bcx.ins().icmp(IntCC::Equal, tag, want);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let prot_bits = bcx.ins().band_imm(prot, need.cast_signed());
    let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, prot_bits, 0);
    let host = bcx.ins().iadd(base, page_off);

    guard_exit(bcx, in_page, fail);
    guard_exit(bcx, key_ok, fail);
    (prot_ok, host)
}

// ── The hoisted guard: publish once per block, validate per access ──────
//
// The two probes above re-derive, on **every** memory operand, facts that only
// change when the guest maps or protects memory: which stage answers the
// operand, that stage's guest bounds, its host bias, and its R/W bits. Measured
// from the emitted aarch64, that validation is 15–16 host instructions per
// operand per iteration and it is the *same* 15–16 in every hot block measured
// (`docs/architecture/scaled-blit-block-attribution.md` §8.3, §9.4).
//
// The functions below publish those facts once, in the prologue, as a
// [`HomeSpan`] — a guest base, a length and a host bias — and validate an access
// against it with one subtract and one compare. The bias costs nothing per access
// either: the backend folds `addr + bias` into the access's own addressing mode.
// Measured on a real four-operand guest block (`0x1800cef0b`), the emitted guard
// is 6 host instructions per operand against today's 15 — and 5 for the
// single-compare form here, because the two bounds now cost one register each
// instead of four instructions of testing. Nothing here weakens the probe: on
// any failed predicate the caller jumps into the **unchanged** chain
// (`hoisted_pin_probe` → `refill_tlb_probe` → `wie_jit_load`/`wie_jit_store`),
// which re-derives everything from scratch and reports faults exactly as it does
// today.
//
// # What the hoist is allowed to assume, and why it is not new
//
// Today's probe already uses descriptors loaded **once per block** — `live`,
// `guest_base`, `guest_end`, `allow` and the refill `tag`/`base`/`prot` all come
// from `hoist_pin_slot` / `hoist_refill` at block entry, never from a reload
// inside the body. For a self-loop block the block *is* the loop, so the current
// code already relies on "the generation does not move while the loop iterates".
// [`generation_recheck`] turns that argument into a check; it does not add an
// assumption. See `scaled-blit-block-attribution.md` §7.1 for the invalidation
// paths that make it true (`JitCpu::virtual_alloc` / `virtual_free` /
// `virtual_protect` / `mem_map_image`, all of which bump `mem_gen` and
// `invalidate_tlb`).
//
// A failed generation check is *not* a fallback: it goes to the block's exit
// path (state writeback, dispatcher re-entry), because "the mapping moved" means
// the pins and refill slots the prologue read are all suspect, not just one
// operand's.

/// A published stage translation: every guest address in `[lo, lo + len)` maps to
/// the host address `guest + bias`, and every access of the span's `size` bytes
/// starting there is inside the stage.
///
/// `len` is a **count of valid start addresses**, not an upper bound:
/// `len = stage_end - size - stage_lo + 1`, so `addr - lo < len` is exactly
/// today's `addr >= stage_lo && addr + size <= stage_end`. Folding the two
/// bounds into one length is what makes the per-access test a single unsigned
/// compare ([`home_span_probe`]): `t = addr - lo`, fail unless `t < len`.
///
/// That form also subsumes today's `no_wrap` term without a separate test. A
/// wrapped `addr + size` is an `addr` near `u64::MAX`, which is `>= len - 1`
/// above `lo` for any span that fits in the address space, so it fails the same
/// compare — and `len` itself cannot wrap, because it is only non-zero when
/// `stage_end >= stage_lo + size`.
///
/// An **empty** span is simply `len == 0` ([`SPAN_EMPTY_LEN`]): no `t` satisfies
/// `t < 0`, so every access takes the fallback chain. That covers a dead pin, a
/// denied permission bit and a stage shorter than the access, and it is what
/// keeps the loop body single-copy: the *decision* is taken once, in the
/// prologue, and the body carries the answer in one register pair.
///
/// `bias` is `host_base - stage_lo`; on an empty span it is meaningless and
/// unreachable, because no access can match.
#[derive(Clone, Copy)]
pub(super) struct HomeSpan {
    /// Guest base of the stage.
    pub(super) lo: Value,
    /// Number of valid start addresses from `lo`; `0` for an empty span.
    pub(super) len: Value,
    /// `host_base - stage_lo`: `guest + bias` is the host address of `guest`.
    pub(super) bias: Value,
}

/// `lo` of a span that cannot match any address (see [`HomeSpan`]).
///
/// Only `len` decides emptiness, so `lo` is set to zero purely so the pair is
/// readable in a CLIF dump; `addr - 0 < 0` is false for every `addr`.
pub(super) const SPAN_EMPTY_LO: i64 = 0;
/// `len` of a span that cannot match any address (see [`HomeSpan`]).
pub(super) const SPAN_EMPTY_LEN: i64 = 0;

/// Publish one pin descriptor as a [`HomeSpan`] — the prologue half of the hoist
/// for a stack or data pin operand.
///
/// Folds every term of [`hoisted_pin_probe`] that does not depend on the address
/// into the span, so the per-access form is one `sub`, one compare and the
/// `bias` add — which folds into the access's addressing mode:
///
/// | [`hoisted_pin_probe`] term | here |
/// | --- | --- |
/// | `live` (`host_base != 0 && pin_gen == mem_gen`) | entry-time `select` |
/// | `allow & need != 0` (the R/W bit) | entry-time `select` |
/// | `no_wrap` (`addr + size` did not overflow) | subsumed by `len` |
/// | `lo_ok` / `hi_ok` | retained: the folded interval test in [`home_span_probe`] |
///
/// Folding `live` and the R/W bit is exact, not an approximation: both are
/// functions of values the current probe reads once per block anyway, and a
/// change to either one is a mapping or protection change, which bumps
/// `mem_gen` and takes the block out through [`generation_recheck`].
///
/// `size` is the *operand's* access width, so one block publishes one span per
/// operand; `write` selects the permission bit the operand needs. The span is
/// loop-invariant (it is built entirely from the hoisted descriptor), so it needs
/// no block parameter on the back edge.
pub(super) fn span_of_pin(
    bcx: &mut FunctionBuilder<'_>,
    pin: &HoistedPin,
    size: u32,
    write: bool,
) -> HomeSpan {
    let size_v = iconst_u64(bcx, u64::from(size));
    let lo = pin.guest_base;
    let end = pin.guest_end;
    // `end >= lo + size` ⇔ `len = end - size - lo + 1 >= 1`, i.e. the stage can
    // hold the whole access. Without it a degenerate stage (`end < size`) would
    // compute `len` by wrapping, and a wrapped length is a huge accept window —
    // the one direction that must never be wrong.
    let span = bcx.ins().isub(end, lo);
    let long_enough = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, span, size_v);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let bits = bcx.ins().band_imm(pin.allow, need.cast_signed());
    let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, bits, 0);
    let ok = bcx.ins().band(pin.live, long_enough);
    let ok = bcx.ins().band(ok, prot_ok);
    // `len = end - size - lo + 1`, derived from `span` so the prologue pays one
    // subtract and one increment rather than a second bound.
    let len = bcx.ins().isub(span, size_v);
    let len = bcx.ins().iadd_imm(len, 1);
    HomeSpan {
        lo: select_or_empty(bcx, ok, lo, SPAN_EMPTY_LO),
        len: select_or_empty(bcx, ok, len, SPAN_EMPTY_LEN),
        bias: bcx.ins().isub(pin.host_base, lo),
    }
}

/// One refill slot, loaded and validated once: the prologue half of the hoist
/// for an operand the direct-mapped table answers.
///
/// Exactly [`refill_tlb_probe`]'s first half — index, three slot loads, the
/// folded tag compare, the R/W bit — with the guards replaced by an `ok` value
/// that [`span_of_slot`] turns into a [`HomeSpan`]. The page-crossing term stays
/// where it was: it is the `hi` clamp of the *span*, so an access that would run
/// past the end of the page fails the interval test and takes the chain, just as
/// `page_off + size <= PAGE_SIZE` refuses it today.
///
/// Unlike a pin's, this descriptor is **not** loop-invariant: it is resolved for
/// one page (`page_base`), so a caller that publishes it at entry must republish
/// it — `page_base`, `base` — from the chain's hit block when an access leaves
/// that page. Eviction of the slot is *not* a republish event: the home holds the
/// page's host base, which a conflict cannot move.
///
/// # Contract
///
/// A slot-derived span is only as fresh as the slot tag, and the tag is compared
/// against `refill.mem_gen`, the frame's generation as stamped at `run_compiled`
/// entry. Inside one frame nothing rewrites `JitCtx::mem_gen` (the only writer is
/// the dispatcher), so the published span is exactly as strong as today's
/// per-access tag compare. Hoisting it is therefore sound under the *same*
/// invalidation contract the current probe already documents; it is **not**
/// sound if a helper call inside the block could map or protect guest memory
/// without returning through the dispatcher. None can today (the reachable host
/// code is `wie_jit_*` and `wie_ucrt_*`, and the UCRT helpers only move bytes
/// inside an already-reserved arena), but that is the assumption to re-check if
/// one is ever added.
#[cfg_attr(not(test), expect(dead_code))]
pub(super) fn hoist_refill_slot(
    bcx: &mut FunctionBuilder<'_>,
    refill: &RefillProbe,
    flags: MemFlagsData,
    addr: Value,
    write: bool,
) -> HoistedSlot {
    let page_base = bcx.ins().band_imm(addr, -PAGE_SIZE.cast_signed());
    let page_key = bcx.ins().ushr_imm(addr, 12);
    let h = bcx.ins().imul_imm(page_key, GOLDEN_RATIO_64.cast_signed());
    let shifted = bcx
        .ins()
        .ushr_imm(h, i64::from(REFILL_SHIFT - REFILL_STRIDE_SHIFT));
    let slot_off = bcx.ins().band_imm(shifted, REFILL_OFFSET_MASK);
    let slot = bcx.ins().iadd(refill.table, slot_off);
    let tag = bcx.ins().load(types::I64, flags, slot, 0);
    let base = bcx.ins().load(types::I64, flags, slot, 8);
    let prot = bcx.ins().load(types::I64, flags, slot, 16);
    // One compare covers page identity *and* mapping generation (see `RefillSlot`).
    let want = bcx.ins().bxor(page_key, refill.mem_gen);
    let tag_ok = bcx.ins().icmp(IntCC::Equal, tag, want);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let bits = bcx.ins().band_imm(prot, need.cast_signed());
    let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, bits, 0);
    HoistedSlot {
        page_base,
        base,
        ok: bcx.ins().band(tag_ok, prot_ok),
    }
}

/// One refill slot's translation as loaded by [`hoist_refill_slot`].
#[cfg_attr(not(test), expect(dead_code))]
#[derive(Clone, Copy)]
pub(super) struct HoistedSlot {
    /// `addr & !0xFFF` at publish time — the guest page the slot answers.
    pub(super) page_base: Value,
    /// `slot.base`: host base of that page.
    pub(super) base: Value,
    /// `tag == page_key ^ mem_gen && (prot & need) != 0` at publish time.
    pub(super) ok: Value,
}

/// Turn a [`HoistedSlot`] into a [`HomeSpan`] for an access of `size` bytes.
///
/// The interval is exactly "inside this page with room for the access" — today's
/// `in_page` term — and `bias` is the page's host base minus its guest base.
///
/// A slot home is one page wide, so its `len` is a **compile-time constant**
/// (`PAGE_SIZE - size + 1`): the per-access guard carries no bound computation
/// at all, just the compare against the loaded `lo`.
#[cfg_attr(not(test), expect(dead_code))]
pub(super) fn span_of_slot(
    bcx: &mut FunctionBuilder<'_>,
    slot: &HoistedSlot,
    size: u32,
) -> HomeSpan {
    let len = iconst_u64(
        bcx,
        PAGE_SIZE.saturating_sub(u64::from(size)).saturating_add(1),
    );
    HomeSpan {
        lo: select_or_empty(bcx, slot.ok, slot.page_base, SPAN_EMPTY_LO),
        len: select_or_empty(bcx, slot.ok, len, SPAN_EMPTY_LEN),
        bias: bcx.ins().isub(slot.base, slot.page_base),
    }
}

/// `select(ok, v, empty)` — the empty arm is an immediate, so the backend folds
/// it into the `csel`.
fn select_or_empty(bcx: &mut FunctionBuilder<'_>, ok: Value, v: Value, empty: i64) -> Value {
    let e = bcx.ins().iconst(types::I64, empty);
    bcx.ins().select(ok, v, e)
}

/// Validate one access against a published [`HomeSpan`]; returns the host address.
///
/// One subtract, one unsigned compare and an add, as a guarded exit to `fail` —
/// the *unchanged* probe chain for this operand, which re-derives the translation
/// from scratch:
///
/// ```text
/// t    = addr - lo            ; unsigned distance from the stage base
/// ok   = t < len              ; inside the stage, with room for the access
/// host = addr + bias          ; only meaningful on the true edge
/// ```
///
/// The single compare accepts exactly the addresses [`hoisted_pin_probe`]
/// accepts (`addr >= guest_base && addr + size <= guest_end`, with the width
/// folded into `len`) and exactly the addresses [`refill_tlb_probe`] accepts
/// (`page_off + size <= PAGE_SIZE`, with the page folded out). It also subsumes
/// the wrap check: a `addr + size` overflow puts `addr` more than `len - 1` above
/// `lo`, which is the same `ok == false`. The earlier conditions — `live` and the
/// R/W bit — are in the span; see [`span_of_pin`].
///
/// Two instructions per access plus the two the bounds cost to load, and one
/// fewer branch than the two-sided form it replaces: `sub`, `cmp`, `brif` against
/// `cmp`, `brif`, `cmp`, `brif`.
///
/// `host` is defined in the block the guard returns in, so it dominates the
/// caller's hit block, the same contract as [`hoisted_pin_probe`].
pub(super) fn home_span_probe(
    bcx: &mut FunctionBuilder<'_>,
    span: &HomeSpan,
    addr: Value,
    fail: Block,
) -> Value {
    let dist = bcx.ins().isub(addr, span.lo);
    let ok = bcx.ins().icmp(IntCC::UnsignedLessThan, dist, span.len);
    let host = bcx.ins().iadd(addr, span.bias);
    guard_exit(bcx, ok, fail);
    host
}

/// The mapping generation the spans were published under is still current.
///
/// Returns a boolean that is **true while the home is valid**. Emitted once per
/// iteration and shared by every operand of the block (the generation is global;
/// a per-operand compare would be one per operand for the same answer), and its
/// failure edge is the block's exit path, not an operand's fallback: when the
/// mapping moved, every descriptor the prologue read is suspect.
///
/// # What this check observes — and why the wiring does **not** use it
///
/// It compares `JitCtx::mem_gen` as it stands at the access against the value the
/// prologue loaded. The dispatcher is the only writer of that field
/// (`pipeline.rs`, `run_compiled`), so inside one `run_compiled` frame the two
/// agree by construction: the compare can fire only if something re-stamps the
/// field mid-frame, and nothing does. It is a checked assumption costing three
/// instructions per iteration, not a live observation of
/// `GuestMemory::generation` (a guest map/protect bumps the *shared* atomic, not
/// the frame's copy).
///
/// Two tests settle what it is worth, and both are in `mem_hoist_tests.rs`:
///
/// * `a_reprotect_between_executions_is_caught` — a real `VirtualProtect` between
///   two executions of the same compiled block *is* caught: the prologue re-reads
///   the descriptor and the generation on every entry, so execution 2 publishes
///   from a descriptor that lost `W` and refuses the store. Its negative control
///   (run once by hand, never committed) removed the dispatcher entry and kept
///   the stale descriptor: the span was then taken and the store landed through a
///   page the guest had made read-only — a silent protection violation, and the
///   reason that per-entry reload is load-bearing rather than incidental.
/// * `only_an_in_frame_move_needs_the_generation_compare` — with the compare the
///   access exits; without it the access is served. So it buys exactly the case
///   that cannot happen.
///
/// Hence the recommended wiring calls [`home_span_probe`], not this: the
/// protection case is covered by the per-entry reload that already exists and is
/// now tested, and the three instructions per iteration are pure cost. This
/// function stays for a caller that *can* have a moving generation inside its own
/// code region (the region lane, which already emits an invalidation guard on its
/// back edge), where piggybacking is cheaper than a second mechanism.
#[cfg_attr(not(test), expect(dead_code))]
pub(super) fn generation_recheck(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gen_at_entry: Value,
) -> Value {
    let p = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_MEM_GEN));
    let now = bcx.ins().load(types::I64, flags, p, 0);
    bcx.ins().icmp(IntCC::Equal, now, gen_at_entry)
}

/// One operand's per-access validation, generation check included: the composed
/// form of [`generation_recheck`] + [`home_span_probe`].
///
/// `stale` (the generation moved) is a separate edge from `fail` (this operand is
/// not in the stage): the first is the block's exit path, the second is this
/// operand's own probe chain. Returns the host address, defined in the block both
/// guard chains return in.
#[cfg_attr(not(test), expect(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(super) fn hoisted_access_probe(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    span: &HomeSpan,
    gen_at_entry: Value,
    addr: Value,
    stale: Block,
    fail: Block,
) -> Value {
    let current = generation_recheck(bcx, ctx_ptr, flags, gen_at_entry);
    guard_exit(bcx, current, stale);
    home_span_probe(bcx, span, addr, fail)
}

/// One published home: `(access width, direction, span)`.
///
/// The key is the pair, not the instruction: `call_load` reads the
/// `(size, false)` entry and `call_store` the `(size, true)` one, so two operands
/// of the same width share a home, and a block whose operands are all one width
/// pays for exactly two spans.
pub(super) type HomeEntry = (u32, bool, HomeSpan);

/// The distinct access widths of a block's memory operands — the input to
/// [`block_homes`].
///
/// **Widths only.** The direction is deliberately *not* inferred here: a wrong
/// direction would publish a span carrying the wrong permission bit, and that is
/// the one thing this design must never get wrong. Publishing both directions per
/// width is exact by construction (each is built with its own `need`), while an
/// unused pair costs a few words of prologue text rather than a wrong answer.
///
/// iced's `InstrInfo` is the decoder's own table, so "does this instruction touch
/// memory, and how wide" is read rather than guessed. The result is an
/// over-approximation of what the lowering will actually access (implicit stack
/// operands of `push`/`call` included): publishing a home nothing uses is dead
/// text, and *missing* a home only means that operand keeps today's probe.
///
/// The block's terminator is not in `insns`; a `call [mem]` operand is therefore
/// not hoisted. That is a missed opportunity on a shape that is not a hot
/// self-loop, not a hole.
pub(super) fn block_home_sizes(insns: &[DecodedInsn]) -> Vec<u32> {
    let mut factory = iced_x86::InstructionInfoFactory::new();
    let mut sizes: Vec<u32> = Vec::new();
    for insn in insns {
        // `used_memory()` is the decoder's own list of the memory locations an
        // instruction touches; an instruction with none is skipped, and a `push`
        // that only *implicitly* touches the stack still counts (its widths are
        // then published and possibly unused, which is free).
        let sizes_here: Vec<u32> = {
            let info = factory.info(&insn.instr);
            info.used_memory()
                .iter()
                .map(|m| u32::try_from(m.memory_size().size()).unwrap_or(0))
                .collect()
        };
        let Some(size) = sizes_here.into_iter().find(|s| matches!(s, 1 | 2 | 4 | 8)) else {
            continue;
        };
        if matches!(size, 1 | 2 | 4 | 8) && !sizes.contains(&size) {
            sizes.push(size);
            if sizes.len() == 4 {
                break;
            }
        }
    }
    sizes
}

/// Publish one [`HomeSpan`] per `(width, direction)` pair from a hoisted pin
/// descriptor — the prologue half of the hoist, emitted once per block entry.
///
/// Sixteen host instructions per span (measured: see the report accompanying
/// this change), so a one-width loop pays ~32 at entry. Nothing here is
/// loop-carried: a pin-derived span is a function of the entry-loaded descriptor
/// alone, so the back edge's block-parameter contract is untouched.
pub(super) fn block_homes(
    bcx: &mut FunctionBuilder<'_>,
    pin: Option<HoistedPin>,
    insns: &[DecodedInsn],
    self_loop: bool,
) -> Vec<HomeEntry> {
    // The three ways a block gets no guard, all decided here so a call site cannot
    // get the policy wrong: no descriptor (inline mem off, or the stack super path,
    // which emits bare host memops and must not carry one), a block that is not a
    // self-loop (it runs once, so a prologue span cannot be amortised), or no
    // memory width the guard could serve. All three fall through to exactly today's
    // code.
    let Some(pin) = pin.filter(|_| self_loop) else {
        return Vec::new();
    };
    let sizes = block_home_sizes(insns);
    if sizes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(sizes.len() * 2);
    for size in sizes {
        for write in [false, true] {
            out.push((size, write, span_of_pin(bcx, &pin, size, write)));
        }
    }
    out
}

/// Attach the published homes to this block's probe descriptor.
///
/// A no-op when there is no descriptor — which is what makes `WIE_JIT_MEM=slow`,
/// the stack super path, region mode and memory-free blocks all fall out
/// *structurally* rather than by four sites remembering to pass `Vec::new()`.
pub(super) fn attach_homes(
    refill: &Option<RefillProbe>,
    homes: Vec<HomeEntry>,
) -> Option<RefillProbe> {
    refill.as_ref().map(|r| RefillProbe {
        table: r.table,
        mem_gen: r.mem_gen,
        homes,
    })
}

/// The home published for this operand, or `None` for today's code path.
///
/// A linear scan over at most eight entries, at **emission** time only: it costs
/// nothing on the emitted path.
pub(super) fn home_for(refill: Option<&RefillProbe>, width: u32, write: bool) -> Option<HomeSpan> {
    refill?
        .homes
        .iter()
        .find(|(w, dir, _)| *w == width && *dir == write)
        .map(|(_, _, span)| *span)
}

/// Emit one operand's hoisted access **with** the in-frame generation check, its
/// hit block, and control continuing in the **fallback block**, where the caller
/// emits the operand's unchanged chain and jumps to the same `merge`.
///
/// The recommended wiring does not use this: it calls [`home_span_probe`] and
/// keeps the chain as the only failure edge, which costs three fewer instructions
/// per iteration. See [`generation_recheck`] for the two tests that decide that.
///
/// This is [`home_span_probe`]/[`hoisted_access_probe`] plus the CFG plumbing
/// [`emit_probe_stage`] already does for a probe, so the call-site change inside
/// `call_load`/`call_store` is one inserted call rather than a new chain:
///
/// ```text
/// let merge = bcx.create_block();            // exists already
/// if let Some(span) = mem.home {             // per operand, published at entry
///     hoisted_stage(bcx, span, gen, ctx_ptr, flags, addr, merge, |bcx| exit(bcx), hit);
/// }
/// // … then today's `emit_probe_stage` chain for this operand, unchanged, which
/// // now emits in the block this function left the builder on …
/// ```
///
/// `on_stale` must **terminate** the block it is given (flush state, return to the
/// dispatcher) — the same contract `emit_inv_gen_check` gives its own handler. The
/// `hit_effect` closure is [`emit_probe_stage`]'s: it performs the access in the
/// hit block and returns the value to thread to `merge`, if any.
///
/// # `WIE_JIT_MEM=slow`
///
/// A caller must pass `None` (and therefore skip this stage entirely) whenever
/// `MemEnv::refill` is `None`, which is what that knob sets: no span is ever
/// published, so the emitted code is byte-for-byte today's.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), expect(dead_code))]
pub(super) fn hoisted_stage(
    bcx: &mut FunctionBuilder<'_>,
    span: &HomeSpan,
    gen_at_entry: Value,
    ctx_ptr: Value,
    flags: MemFlagsData,
    addr: Value,
    merge: Block,
    on_stale: impl FnOnce(&mut FunctionBuilder<'_>),
    hit_effect: impl FnOnce(&mut FunctionBuilder<'_>, Value) -> Option<Value>,
) {
    let stale = bcx.create_block();
    let fail = bcx.create_block();
    let host = hoisted_access_probe(bcx, ctx_ptr, flags, span, gen_at_entry, addr, stale, fail);
    // Positioned in the guard's continuation: this is the hit block.
    match hit_effect(bcx, host) {
        Some(v) => bcx.ins().jump(merge, &[BlockArg::Value(v)]),
        None => bcx.ins().jump(merge, &[]),
    };
    bcx.switch_to_block(stale);
    bcx.seal_block(stale);
    on_stale(bcx);
    bcx.switch_to_block(fail);
    bcx.seal_block(fail);
}

/// Zero-extend a loaded integer of `size` bytes to i64.
///
/// `flags` should be the caller's guest-data alias-tagged flags (`mem.guest_flags`)
/// so this load doesn't alias JitCtx metadata for Cranelift's alias analysis.
pub(super) fn load_guest_bytes(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    host: Value,
    size: u32,
) -> Value {
    match size {
        1 => {
            let v = bcx.ins().load(types::I8, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        2 => {
            let v = bcx.ins().load(types::I16, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        4 => {
            let v = bcx.ins().load(types::I32, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        _ => bcx.ins().load(types::I64, flags, host, 0),
    }
}

pub(super) fn store_guest_bytes(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    host: Value,
    size: u32,
    value: Value,
) {
    match size {
        1 => {
            let v = bcx.ins().ireduce(types::I8, value);
            bcx.ins().store(flags, v, host, 0);
        }
        2 => {
            let v = bcx.ins().ireduce(types::I16, value);
            bcx.ins().store(flags, v, host, 0);
        }
        4 => {
            let v = bcx.ins().ireduce(types::I32, value);
            bcx.ins().store(flags, v, host, 0);
        }
        _ => {
            bcx.ins().store(flags, value, host, 0);
        }
    }
}

pub(super) fn call_load(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    insn_ip: u64,
) -> Result<Value, LowerError> {
    let load_ref = mem
        .load_ref
        .ok_or_else(|| LowerError::verifier("internal: load helper not declared"))?;
    // `WIE_JIT_MEM=slow`: helper only (oracle / bisect).
    if !JitConfig::get().mem_inline_enabled() {
        return Ok(emit_load_helper(
            bcx, mem, gpr, rflags, addr, size, insn_ip, load_ref,
        ));
    }

    // Block-wide super-fast path: entry guard already proved the whole access
    // range sits in the stack pin — emit a bare host load (no bounds IR).
    if let Some(super_s) = mem.super_stack {
        let host = bcx.ins().iadd(super_s.bias, addr);
        return Ok(load_guest_bytes(bcx, mem.guest_flags, host, size));
    }

    // CFG-ordered probes (not `select`).
    // Order: stack → refill table → largest data pin → helper.
    // The refill probe first keeps single-page streams free of pin-bounds IR;
    // the data pin catches multi-page thrash inside VirtualAlloc / heap after
    // the refill slot was replaced by a conflicting page.
    let merge = bcx.create_block();
    bcx.append_block_param(merge, types::I64);

    // Hoisted fast path: a home published at entry from the stack-pin descriptor.
    // On *any* failed predicate this falls into the chain below, which validates
    // everything from scratch — the hoist only adds an edge into it, and the
    // descriptor it was published from is re-read on every block entry (which is
    // what catches a re-protect; see `mem_hoist_tests::a_reprotect_between_executions_is_caught`).
    // A home is the stack pin published at entry, and `home_span_probe` accepts
    // exactly what `hoisted_pin_probe` accepts. After a home miss the stack stage
    // below therefore cannot hit, so it is not emitted: it was a dozen dead
    // instructions in front of every heap or image access.
    let home = home_for(mem.refill.as_ref(), size, false);
    if let Some(span) = home {
        let fail = bcx.create_block();
        let host = home_span_probe(bcx, &span, addr, fail);
        let v = load_guest_bytes(bcx, mem.guest_flags, host, size);
        bcx.ins().jump(merge, &[BlockArg::Value(v)]);
        bcx.switch_to_block(fail);
        bcx.seal_block(fail);
    }

    // Then the page this site used last (loops only).
    let site = mem.sites.as_ref().and_then(|sites| sites.take());
    if let Some(site) = site {
        site.probe(bcx, addr, size, merge, |bcx, host| {
            Some(load_guest_bytes(bcx, mem.guest_flags, host, size))
        });
    }

    if let Some(ref pin) = mem.stack_pin.filter(|_| home.is_none()) {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| hoisted_pin_probe(bcx, pin, addr, size, false, miss),
            |bcx, host| Some(load_guest_bytes(bcx, mem.guest_flags, host, size)),
        );
    }

    if let Some(refill) = mem.refill.as_ref() {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| refill_tlb_probe(bcx, refill, mem.flags, addr, size, false, miss),
            |bcx, host| {
                if let Some(site) = site {
                    site.fill(bcx, addr, host);
                }
                Some(load_guest_bytes(bcx, mem.guest_flags, host, size))
            },
        );
    }

    for pin in &mem.data_pins {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| hoisted_pin_probe(bcx, pin, addr, size, false, miss),
            |bcx, host| Some(load_guest_bytes(bcx, mem.guest_flags, host, size)),
        );
    }

    let slow_val = emit_load_helper(bcx, mem, gpr, rflags, addr, size, insn_ip, load_ref);
    bcx.ins().jump(merge, &[BlockArg::Value(slow_val)]);

    bcx.switch_to_block(merge);
    bcx.seal_block(merge);
    Ok(bcx.block_params(merge)[0])
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
/// Emit one CFG-ordered probe stage of the load/store miss chain.
///
/// `probe` receives the stage's `miss` block and emits the probe's conditions as
/// guarded exits to it; it returns `(last_condition, host)`, which this function
/// branches on into the stage's `hit` block. `hit` runs `hit_effect` and jumps
/// to `merge` (threading its optional value as a merge arg); control then
/// continues in the sealed `miss` block, so the next stage (or the slow-path
/// helper) is emitted there. Monomorphized closures keep this
/// compile-path-only — emitted IR is identical per call site.
///
/// The guard is *control flow*, not a materialised boolean: see [`guard_exit`]
/// for why that is the same decision at fewer host instructions.
fn emit_probe_stage(
    bcx: &mut FunctionBuilder<'_>,
    merge: Block,
    probe: impl FnOnce(&mut FunctionBuilder<'_>, Block) -> (Value, Value),
    hit_effect: impl FnOnce(&mut FunctionBuilder<'_>, Value) -> Option<Value>,
) {
    let hit = bcx.create_block();
    let miss = bcx.create_block();
    let (last, host) = probe(bcx, miss);
    bcx.ins().brif(last, hit, &[], miss, &[]);
    bcx.switch_to_block(hit);
    bcx.seal_block(hit);
    match hit_effect(bcx, host) {
        Some(v) => bcx.ins().jump(merge, &[BlockArg::Value(v)]),
        None => bcx.ins().jump(merge, &[]),
    };
    bcx.switch_to_block(miss);
    bcx.seal_block(miss);
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_helper(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    insn_ip: u64,
    load_ref: FuncRef,
) -> Value {
    let size_v = bcx.ins().iconst(types::I64, i64::from(size));
    let ip_v = iconst_u64(bcx, insn_ip);
    let call = bcx.ins().call(load_ref, &[mem.ctx_ptr, addr, size_v, ip_v]);
    let slow_val = bcx.inst_results(call)[0];
    check_fault_after_ucrt(bcx, mem, gpr, rflags);
    if let Some(sites) = mem.sites.as_ref() {
        sites.clear(bcx);
    }
    slow_val
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn call_store(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    value: Value,
    insn_ip: u64,
) -> Result<(), LowerError> {
    let store_ref = mem
        .store_ref
        .ok_or_else(|| LowerError::verifier("internal: store helper not declared"))?;
    if !JitConfig::get().mem_inline_enabled() {
        emit_store_helper(bcx, mem, gpr, rflags, addr, size, value, insn_ip, store_ref);
        return Ok(());
    }

    // Block-wide super-fast path (see `call_load`).
    if let Some(super_s) = mem.super_stack {
        let host = bcx.ins().iadd(super_s.bias, addr);
        store_guest_bytes(bcx, mem.guest_flags, host, size, value);
        return Ok(());
    }

    // Same CFG order as `call_load`: stack → refill table → data pin → helper.
    let merge = bcx.create_block();

    // Hoisted fast path (see `call_load`): the write-direction home.
    // A home is the stack pin published at entry, and `home_span_probe` accepts
    // exactly what `hoisted_pin_probe` accepts. After a home miss the stack stage
    // below therefore cannot hit, so it is not emitted: it was a dozen dead
    // instructions in front of every heap or image access.
    let home = home_for(mem.refill.as_ref(), size, true);
    if let Some(span) = home {
        let fail = bcx.create_block();
        let host = home_span_probe(bcx, &span, addr, fail);
        store_guest_bytes(bcx, mem.guest_flags, host, size, value);
        bcx.ins().jump(merge, &[]);
        bcx.switch_to_block(fail);
        bcx.seal_block(fail);
    }

    // Then the page this site used last (loops only).
    let site = mem.sites.as_ref().and_then(|sites| sites.take());
    if let Some(site) = site {
        site.probe(bcx, addr, size, merge, |bcx, host| {
            store_guest_bytes(bcx, mem.guest_flags, host, size, value);
            None
        });
    }

    if let Some(ref pin) = mem.stack_pin.filter(|_| home.is_none()) {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| hoisted_pin_probe(bcx, pin, addr, size, true, miss),
            |bcx, host| {
                store_guest_bytes(bcx, mem.guest_flags, host, size, value);
                None
            },
        );
    }

    if let Some(refill) = mem.refill.as_ref() {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| refill_tlb_probe(bcx, refill, mem.flags, addr, size, true, miss),
            |bcx, host| {
                if let Some(site) = site {
                    site.fill(bcx, addr, host);
                }
                store_guest_bytes(bcx, mem.guest_flags, host, size, value);
                None
            },
        );
    }

    for pin in &mem.data_pins {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| hoisted_pin_probe(bcx, pin, addr, size, true, miss),
            |bcx, host| {
                store_guest_bytes(bcx, mem.guest_flags, host, size, value);
                None
            },
        );
    }

    emit_store_helper(bcx, mem, gpr, rflags, addr, size, value, insn_ip, store_ref);
    bcx.ins().jump(merge, &[]);

    bcx.switch_to_block(merge);
    bcx.seal_block(merge);
    Ok(())
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_helper(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    value: Value,
    insn_ip: u64,
    store_ref: FuncRef,
) {
    let size_v = bcx.ins().iconst(types::I64, i64::from(size));
    let ip_v = iconst_u64(bcx, insn_ip);
    bcx.ins()
        .call(store_ref, &[mem.ctx_ptr, addr, size_v, value, ip_v]);
    check_fault_after_ucrt(bcx, mem, gpr, rflags);
    if let Some(sites) = mem.sites.as_ref() {
        sites.clear(bcx);
    }
}

// ── Probe-guard decision table ──────────────────────────────────────────

/// The probe guards are the memory fast path's only decision: whether the stack
/// pin, the refill slot, or the helper answers an access. They are emitted as a
/// chain of conditional branches ([`guard_exit`]) instead of a materialised
/// `i1` conjunction, and these tests hold that expression to the conjunction's
/// meaning — one row per predicate combination, on the real JIT and the real
/// probe IR, not on a model of it.
///
/// Every assertion is on an **observable of the decision**, never on how it was
/// compiled:
///
/// * the value the access produced — a guard that wrongly accepts the stack pin
///   reads through a displaced host pointer, so it returns another page's bytes;
/// * whether the host helper ran (`ctx.load_calls` / `ctx.store_calls`) — the
///   only other way to translate, so `0` means an inline probe answered and `1`
///   means none did.
///
/// Each row asserts **both**, because the two failure directions are different
/// bugs: a guard that accepts too much is a wrong answer or an unchecked
/// access, and one that accepts too little is a silent return to the helper.
///
/// Rows needing one stage in isolation blind the other: `ctx.pins[0] =
/// MemPin::EMPTY` takes the stack-pin probe out of the chain, and emptying
/// `ctx.refill` takes the refill probe out. Both stay as written between runs
/// because `ctx.pins` is rebuilt only when `GuestMemory`'s generation moves
/// (see `pipeline.rs`), which is what makes single-false-condition rows
/// possible at all.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod guard_tests {
    use super::super::{GuestVa, HostAddr, MemPin, REFILL_SLOTS, RefillSlot, refill_slot};
    use super::*;
    use crate::exec::StepResult;
    use crate::mem::protect;
    use crate::mem::{MEM_COMMIT, MEM_RESERVE};
    use crate::{CpuEngine, JitCpu};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Where the block jumps once its access is done: the dispatcher stops
    /// before executing it, so the `ud2` there never runs.
    const STOP_OFF: u64 = 0x100;
    /// `mov rax,[r15]` / `mov [r15],rax`.
    const LOAD64_R15: [u8; 3] = [0x49, 0x8b, 0x07];
    const STORE64_R15: [u8; 3] = [0x49, 0x89, 0x07];
    /// Distinct code VA per distinct encoding: the interpreter's decode cache is
    /// process-wide and keyed by RIP, so two tests handing different bytes to
    /// the same VA would silently execute the first one's instruction.
    const CODE_BASE: u64 = 0x6900_0000;
    /// `MEM_RESERVE` rounds the base and the size to the 64 KiB allocation
    /// granularity, so one stride per page keeps the fixtures disjoint.
    const CODE_STRIDE: u64 = 0x1_0000;
    static NEXT_CODE_PAGE: AtomicU64 = AtomicU64::new(0);

    /// Whether this process emits the inline probe stages these tests exercise.
    ///
    /// `WIE_JIT_MEM=slow` lowers every access straight to the helper, and
    /// `WIE_JIT_MEM=pin` adds data-pin probes after the refill stage, which can
    /// answer an access the row expects to reach the helper. Both are bisect
    /// paths, not the shape under test.
    fn probe_ir_active() -> bool {
        JitConfig::get().mem_inline_enabled() && !JitConfig::get().mem_pin_enabled()
    }

    fn code_page(cpu: &mut JitCpu, insn: [u8; 3]) -> u64 {
        let idx = NEXT_CODE_PAGE.fetch_add(1, Ordering::Relaxed);
        let base = CODE_BASE.saturating_add(idx.saturating_mul(CODE_STRIDE));
        cpu.virtual_alloc(
            base,
            usize::try_from(CODE_STRIDE).expect("stride fits"),
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("map the fixture's code page");
        let stop = usize::try_from(STOP_OFF).expect("stop offset fits");
        let mut page = vec![0x90_u8; stop + 2];
        page[0..3].copy_from_slice(&insn);
        // `jmp rel32` to STOP, from +3 to +8.
        let rel = i32::try_from(STOP_OFF.saturating_sub(8)).expect("rel32 fits");
        page[3] = 0xe9;
        page[4..8].copy_from_slice(&rel.to_le_bytes());
        page[stop] = 0x0f;
        page[stop + 1] = 0x0b;
        cpu.mem_write(base, &page).expect("write the fixture bytes");
        base
    }

    fn map_marked(cpu: &mut JitCpu, base: u64, pages: usize) {
        let size = pages.saturating_mul(0x1000);
        // `PAGE_READWRITE`, not `PAGE_EXECUTE_READWRITE`: the memory layer models
        // an executable mapping as read-only for the guest (W^X), and the store
        // rows below need a page whose permission word really carries W.
        cpu.virtual_alloc(
            base,
            size,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("map the data region");
        for i in 0..pages {
            let va = base.saturating_add(u64::try_from(i).unwrap_or(0).saturating_mul(0x1000));
            cpu.mem_write(va, &u64::try_from(i).unwrap_or(0).to_ne_bytes())
                .expect("stamp the page marker");
        }
    }

    /// Step the block at `code` once with `r15 = addr`, returning `(rax, calls)`
    /// where `calls` is the helper invocation count for this run.
    fn step(cpu: &mut JitCpu, code: u64, addr: u64) -> (Option<StepResult>, u64) {
        let regs = &mut cpu.thread.regs;
        regs.set_gpr(15, addr);
        regs.set_rflags_checked(Rflags::from(0x202_u64));
        cpu.write_rip(code).expect("set rip");
        let (result, _retired) = cpu.step_one().expect("step the fixture block");
        let calls = cpu
            .thread
            .ctx
            .load_calls
            .saturating_add(cpu.thread.ctx.store_calls);
        if matches!(result, StepResult::Continue) {
            assert_eq!(
                cpu.thread.regs.rip,
                code.saturating_add(STOP_OFF),
                "the block must run to STOP"
            );
        }
        (Some(result), calls)
    }

    /// One table row: a load of `want` through `calls` helper calls.
    fn expect_load(cpu: &mut JitCpu, code: u64, addr: u64, want: u64, calls: u64, row: &str) {
        let (result, got_calls) = step(cpu, code, addr);
        let rax = cpu.thread.regs.gpr(0);
        assert!(
            matches!(result, Some(StepResult::Continue)),
            "{row}: r15={addr:#x} must retire, got {result:?}"
        );
        assert_eq!(rax, want, "{row}: r15={addr:#x} read the wrong bytes");
        assert_eq!(
            got_calls, calls,
            "{row}: r15={addr:#x} took {got_calls} helper calls, expected {calls}"
        );
    }

    fn read_u64(cpu: &mut JitCpu, addr: u64) -> u64 {
        let mut buf = [0_u8; 8];
        cpu.mem_read(addr, &mut buf).expect("read u64");
        u64::from_ne_bytes(buf)
    }

    /// Take the stack-pin probe out of the chain, so a row's outcome is the
    /// refill probe's decision.
    fn blind_stack_pin(cpu: &mut JitCpu) {
        cpu.thread.ctx.pins[0] = MemPin::EMPTY;
    }

    /// Take the refill probe out of the chain, so a row's outcome is the
    /// stack-pin probe's decision.
    fn blind_refill(cpu: &mut JitCpu) {
        cpu.thread.ctx.refill = [RefillSlot::EMPTY; REFILL_SLOTS];
    }

    /// The refill guard: `in_page ∧ key_ok ∧ prot_ok`, one row per way it can
    /// fail, plus the all-hold row that is the point of the whole probe.
    #[test]
    fn refill_probe_guard_table_over_predicate_combinations() {
        if !probe_ir_active() {
            return;
        }
        const DATA: u64 = 0x6a00_0000;
        let mut cpu = JitCpu::open_x86_64();
        let load_code = code_page(&mut cpu, LOAD64_R15);
        let store_code = code_page(&mut cpu, STORE64_R15);
        map_marked(&mut cpu, DATA, 2);
        // Straddle bytes: no single page can produce this value, so a cross-page
        // row that wrongly took the inline path cannot pass by accident.
        cpu.mem_write(DATA.saturating_add(0xffc), &0x1122_3344_u32.to_le_bytes())
            .expect("tail of page 0");
        cpu.mem_write(DATA.saturating_add(0x1000), &0x5566_7788_u32.to_le_bytes())
            .expect("head of page 1");
        blind_stack_pin(&mut cpu);

        // Cold: the helper resolves the page and installs the slot. Every row
        // below starts from an installed, valid slot for page 0.
        expect_load(&mut cpu, load_code, DATA, 0, 1, "cold install");
        let page_key = DATA >> 12;
        assert!(
            !cpu.thread.ctx.refill[refill_slot(page_key)].base.is_null(),
            "the cold access must have installed a slot, or the rows below prove nothing"
        );

        // All three hold — the inline path, zero helper calls.
        expect_load(&mut cpu, load_code, DATA, 0, 0, "all predicates hold");

        // `in_page` false, everything else true: 8 bytes at page_end-4. The slot
        // for the first page is warm, so the page-crossing term is the only one
        // that can refuse this access.
        expect_load(
            &mut cpu,
            load_code,
            DATA.saturating_add(0xffc),
            0x5566_7788_1122_3344,
            1,
            "in_page false (cross-page)",
        );

        // `key_ok` false: same page, same permission, stale tag.
        let good_tag = cpu.thread.ctx.refill[refill_slot(page_key)].tag;
        cpu.thread.ctx.refill[refill_slot(page_key)].tag = good_tag ^ 1;
        expect_load(&mut cpu, load_code, DATA, 0, 1, "key_ok false (stale tag)");
        assert_eq!(
            cpu.thread.ctx.refill[refill_slot(page_key)].tag,
            good_tag,
            "the helper must have re-installed the slot under the current tag"
        );

        // `prot_ok` false for a load: the slot says writable-only.
        cpu.thread.ctx.refill[refill_slot(page_key)].prot = TLB_PROT_W;
        expect_load(
            &mut cpu,
            load_code,
            DATA,
            0,
            1,
            "prot_ok false (load, W-only)",
        );
        assert_eq!(
            cpu.thread.ctx.refill[refill_slot(page_key)].prot,
            TLB_PROT_R | TLB_PROT_W,
            "a writable page installs R|W, not just the bit the access needed"
        );

        // `prot_ok` false for a store: the slot says readable-only, so the store
        // must go to the helper — which on the RWX page then writes through.
        cpu.thread.ctx.refill[refill_slot(page_key)].prot = TLB_PROT_R;
        let stored = 0xdead_beef_0bad_f00d_u64;
        cpu.thread.regs.set_gpr(0, stored);
        let (result, calls) = step(&mut cpu, store_code, DATA.saturating_add(8));
        assert!(
            matches!(result, Some(StepResult::Continue)),
            "prot_ok false (store): the store must retire through the helper"
        );
        assert_eq!(calls, 1, "prot_ok false (store): the slot must be refused");
        assert_eq!(
            read_u64(&mut cpu, DATA.saturating_add(8)),
            stored,
            "prot_ok false (store): the helper must still write the bytes"
        );

        // …and the same store is inline once the slot carries W.
        let (result, calls) = step(&mut cpu, store_code, DATA.saturating_add(16));
        assert!(matches!(result, Some(StepResult::Continue)), "warm store");
        assert_eq!(calls, 0, "a store the slot allows must not call the helper");
        assert_eq!(read_u64(&mut cpu, DATA.saturating_add(16)), stored);

        // `in_page` false by wrap-around: `page_off + size` overflows the page,
        // which is the same term the cross-page row exercises but with an
        // address no page can hold — the access must fault, not be taken inline.
        let wrap = u64::MAX.saturating_sub(3);
        let (result, calls) = step(&mut cpu, load_code, wrap);
        assert!(
            matches!(result, Some(StepResult::InvalidMemory(_))),
            "a wrapping access must fault, got {result:?}"
        );
        assert_eq!(calls, 1, "a wrapping access must take the helper");
    }

    /// The stack-pin guard: `live ∧ no_wrap ∧ lo_ok ∧ hi_ok ∧ prot_ok`, one row
    /// per predicate, on a pin the test owns.
    #[test]
    fn pin_probe_guard_table_over_predicate_combinations() {
        if !probe_ir_active() {
            return;
        }
        const DATA: u64 = 0x6a01_0000;
        let mut cpu = JitCpu::open_x86_64();
        let load_code = code_page(&mut cpu, LOAD64_R15);
        let store_code = code_page(&mut cpu, STORE64_R15);
        map_marked(&mut cpu, DATA, 1);
        // One run to build the frame's pins for the current generation
        // (`pipeline.rs` rebuilds them only when the generation moves).
        let _ = step(&mut cpu, load_code, DATA);
        let cur_gen = cpu.thread.ctx.mem_gen;
        let addr = DATA.saturating_add(0x40);
        // The pin under test is the descriptor that covers `addr`: the emitted
        // stack-pin stage probes `pins[0]` whatever it holds, so borrowing a
        // real descriptor into slot 0 tests the probe rather than a fixture that
        // looks like a pin.
        let pristine = *cpu
            .thread
            .ctx
            .pins
            .iter()
            .find(|p| {
                !p.is_empty()
                    && p.guest_base.0 <= addr
                    && addr.saturating_add(8) <= p.guest_end.0
                    && (p.allow & TLB_PROT_R) != 0
            })
            .expect("the fixture needs a pin covering its data page");
        blind_stack_pin(&mut cpu);
        cpu.thread.ctx.pins[0] = pristine;
        let marker = 0x5a5a_1234_5678_9abc_u64;
        cpu.mem_write(addr, &marker.to_ne_bytes())
            .expect("stamp the pinned bytes");

        // ── all five hold ───────────────────────────────────────────────
        blind_refill(&mut cpu);
        cpu.thread.ctx.pins[0] = pristine;
        expect_load(&mut cpu, load_code, addr, marker, 0, "all predicates hold");

        // ── `live` false: no host base ──────────────────────────────────
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.host_base = HostAddr::NULL;
        cpu.thread.ctx.pins[0] = pin;
        expect_load(
            &mut cpu,
            load_code,
            addr,
            marker,
            1,
            "live false (null base)",
        );

        // ── `live` false: stale generation ──────────────────────────────
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.mem_gen = cur_gen ^ 1;
        cpu.thread.ctx.pins[0] = pin;
        expect_load(
            &mut cpu,
            load_code,
            addr,
            marker,
            1,
            "live false (stale gen)",
        );

        // ── `lo_ok` false: address below the pin ────────────────────────
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.guest_base = GuestVa::new(addr.saturating_add(1));
        cpu.thread.ctx.pins[0] = pin;
        expect_load(&mut cpu, load_code, addr, marker, 1, "lo_ok false");

        // ── `hi_ok` false: address at the exclusive end ─────────────────
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.guest_end = GuestVa::new(addr);
        cpu.thread.ctx.pins[0] = pin;
        expect_load(&mut cpu, load_code, addr, marker, 1, "hi_ok false");

        // ── `prot_ok` false: readable-only pin, storing ─────────────────
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.allow = TLB_PROT_R;
        cpu.thread.ctx.pins[0] = pin;
        let stored = 0x0bad_f00d_dead_beef_u64;
        cpu.thread.regs.set_gpr(0, stored);
        let (result, calls) = step(&mut cpu, store_code, addr.saturating_add(8));
        assert!(
            matches!(result, Some(StepResult::Continue)),
            "prot_ok false (store): must retire through the helper"
        );
        assert_eq!(calls, 1, "prot_ok false (store): the pin must be refused");
        assert_eq!(
            read_u64(&mut cpu, addr.saturating_add(8)),
            stored,
            "prot_ok false (store): the helper must still write"
        );

        // …and the same store is inline through a pin that allows writes.
        blind_refill(&mut cpu);
        cpu.thread.ctx.pins[0] = pristine;
        let (result, calls) = step(&mut cpu, store_code, addr.saturating_add(16));
        assert!(
            matches!(result, Some(StepResult::Continue)),
            "warm pin store"
        );
        assert_eq!(calls, 0, "a store the pin allows must not call the helper");

        // ── `no_wrap` false ─────────────────────────────────────────────
        // A pin that covers every address cannot be refused by `live`, `lo_ok`
        // or `hi_ok`; only the wrap term stands between this access and a host
        // pointer computed past the end of the address space.
        blind_refill(&mut cpu);
        let mut pin = pristine;
        pin.guest_base = GuestVa::new(0);
        pin.guest_end = GuestVa::new(u64::MAX);
        pin.allow = TLB_PROT_R | TLB_PROT_W;
        cpu.thread.ctx.pins[0] = pin;
        let wrap = u64::MAX.saturating_sub(3);
        let (result, calls) = step(&mut cpu, load_code, wrap);
        assert!(
            matches!(result, Some(StepResult::InvalidMemory(_))),
            "no_wrap false: a wrapping access must fault, got {result:?}"
        );
        assert_eq!(
            calls, 1,
            "no_wrap false: a wrapping access must take the helper"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
#[path = "mem_hoist_tests.rs"]
mod hoist_tests;
