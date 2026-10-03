//! Hand-written host trampolines for 1–3 instruction guest stubs.
//!
//! Ultra-short fake-API bodies (`ret`, `xor eax,eax; ret`, GetLastError, …)
//! skip Cranelift entirely: lower peak init RAM and cut compile tax on every
//! process start. Semantics match the Cranelift-lowered path (guest `ret`,
//! shadow stack, optional late-bound chain).

use super::ALL_DIRTY_BITS;
use super::block::{BlockTerm, DecodedInsn};
use super::config::JitConfig;
use super::lower::{
    CHAIN_SLOTS, JitCtx, MAX_CHAIN_DEPTH, SHADOW_DEPTH, chain_hash, wie_jit_load, wie_jit_store,
};
use crate::guest_layout::TEB_LAST_ERROR_OFFSET;
use iced_x86::{Mnemonic, OpKind, Register};
use std::sync::atomic::Ordering;

/// Borrow the live [`JitCtx`] behind a raw host pointer.
///
/// This is the **single** place in the JIT that turns a `*mut JitCtx` into a
/// reference: every micro-stub trampoline below, plus the `wie_ucrt_*` fast-API
/// helpers in [`super::fast_api`], route through it, so the liveness invariant
/// is stated once instead of at 17 sites (where it was previously written 5
/// times and silently assumed 12 more).
///
/// # Safety
///
/// The caller must guarantee `raw` is non-null and points at a `JitCtx` that
/// stays alive and is not otherwise borrowed for the whole lifetime `'a`.
///
/// That holds at every call site by the [`MicroStub::func`] / Cranelift-import
/// contract: a stub or fast-API helper is only ever entered from
/// `JitCpu::run_compiled`, which owns the `JitCtx` for the entire native frame
/// and does not touch it until the frame returns. The `'a` is not derived from
/// the raw pointer, so this function must not be used to extend a borrow past
/// the owning frame.
#[must_use]
pub(super) unsafe fn ctx_mut<'a>(raw: *mut JitCtx) -> &'a mut JitCtx {
    // SAFETY: delegated to the caller — see the `# Safety` contract above.
    unsafe { &mut *raw }
}

/// Recognized micro-stub patterns that have a hand-written host trampoline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MicroStub {
    /// Bare `ret`.
    Ret,
    /// `xor eax,eax` / `xor rax,rax` then `ret`.
    ReturnZero,
    /// `mov rax, rcx` then `ret`.
    IdentityRcx,
    /// `mov eax, imm32` then `ret`.
    ReturnImm32(u32),
    /// `mov eax, [gs:0x68]` then `ret` — per-thread TEB last-error load.
    GetLastError,
    /// `mov [gs:0x68], ecx` then `ret` — per-thread TEB last-error store.
    SetLastError,
}

impl MicroStub {
    /// Host entry for this stub (`extern "C" fn(*mut JitCtx)`).
    #[must_use]
    pub(super) fn func(self) -> unsafe extern "C" fn(*mut super::lower::JitCtx) {
        match self {
            Self::Ret => tramp_ret,
            Self::ReturnZero => tramp_return_zero,
            Self::IdentityRcx => tramp_identity_rcx,
            Self::ReturnImm32(imm) => tramp_return_imm32_dispatch(imm),
            Self::GetLastError => tramp_get_last_error,
            Self::SetLastError => tramp_set_last_error,
        }
    }

    /// GPRs that may change and must be written back to the host regfile.
    #[must_use]
    pub(super) fn dirty_mask(self) -> u16 {
        match self {
            Self::Ret | Self::SetLastError => 1 << 4, // RSP only (store uses ECX)
            Self::ReturnZero | Self::ReturnImm32(_) | Self::GetLastError | Self::IdentityRcx => {
                (1 << 0) | (1 << 4) // RAX, RSP
            }
        }
    }

    #[must_use]
    pub(super) fn insn_count(self) -> u32 {
        match self {
            Self::Ret => 1,
            Self::ReturnZero | Self::IdentityRcx | Self::ReturnImm32(_) => 2,
            Self::GetLastError | Self::SetLastError => 2,
        }
    }
}

/// Match a Pure block against a hand-written micro-stub (body + `ret` terminator).
#[must_use]
pub(super) fn match_micro_stub(
    insns: &[DecodedInsn],
    term: Option<BlockTerm>,
) -> Option<MicroStub> {
    if !matches!(term, Some(BlockTerm::Ret)) {
        return None;
    }
    // Terminator `ret` is included in `insns`.
    let n = insns.len();
    if n == 0 || n > 3 {
        return None;
    }
    if !is_plain_ret(&insns[n - 1].instr) {
        return None;
    }
    match n {
        1 => Some(MicroStub::Ret),
        2 => classify_two_insn(&insns[0].instr),
        _ => None,
    }
}

fn is_plain_ret(instr: &iced_x86::Instruction) -> bool {
    instr.mnemonic() == Mnemonic::Ret && instr.op_count() == 0
}

fn classify_two_insn(instr: &iced_x86::Instruction) -> Option<MicroStub> {
    // xor eax,eax / xor rax,rax
    if instr.mnemonic() == Mnemonic::Xor
        && instr.op0_kind() == OpKind::Register
        && instr.op1_kind() == OpKind::Register
    {
        let r0 = instr.op_register(0);
        let r1 = instr.op_register(1);
        if r0 == r1 && matches!(r0, Register::EAX | Register::RAX) {
            return Some(MicroStub::ReturnZero);
        }
    }
    // mov rax, rcx
    if instr.mnemonic() == Mnemonic::Mov
        && instr.op0_kind() == OpKind::Register
        && instr.op1_kind() == OpKind::Register
        && instr.op_register(0) == Register::RAX
        && instr.op_register(1) == Register::RCX
    {
        return Some(MicroStub::IdentityRcx);
    }
    // mov eax, imm32
    if instr.mnemonic() == Mnemonic::Mov
        && instr.op0_kind() == OpKind::Register
        && matches!(
            instr.op1_kind(),
            OpKind::Immediate8
                | OpKind::Immediate16
                | OpKind::Immediate32
                | OpKind::Immediate8to32
                | OpKind::Immediate8to64
                | OpKind::Immediate32to64
        )
        && matches!(instr.op_register(0), Register::EAX | Register::RAX)
    {
        let imm = instr.immediate32();
        return Some(MicroStub::ReturnImm32(imm));
    }
    // mov eax, [gs:TEB_LAST_ERROR_OFFSET] — per-thread TEB last-error load
    // (`GetLastError` guest stub; the GS base resolves per engine at run time).
    if instr.mnemonic() == Mnemonic::Mov
        && instr.op0_kind() == OpKind::Register
        && matches!(instr.op_register(0), Register::EAX | Register::RAX)
        && instr.op1_kind() == OpKind::Memory
        && instr.memory_segment() == Register::GS
        && instr.memory_base() == Register::None
        && instr.memory_index() == Register::None
        && instr.memory_displacement64() == TEB_LAST_ERROR_OFFSET
    {
        return Some(MicroStub::GetLastError);
    }
    // mov [gs:TEB_LAST_ERROR_OFFSET], ecx — per-thread TEB last-error store
    // (`SetLastError` guest stub).
    if instr.mnemonic() == Mnemonic::Mov
        && instr.op0_kind() == OpKind::Memory
        && instr.op1_kind() == OpKind::Register
        && matches!(instr.op_register(1), Register::ECX | Register::RCX)
        && instr.memory_segment() == Register::GS
        && instr.memory_base() == Register::None
        && instr.memory_index() == Register::None
        && instr.memory_displacement64() == TEB_LAST_ERROR_OFFSET
    {
        return Some(MicroStub::SetLastError);
    }
    None
}

// --- Imm32 trampoline table (small fixed set of common constants) ---

/// Common imm32 returns used by guest stubs (TRUE, process/thread ids, tick, locale).
fn tramp_return_imm32_dispatch(imm: u32) -> unsafe extern "C" fn(*mut JitCtx) {
    match imm {
        0 => tramp_return_zero,
        1 => tramp_return_imm_1,
        0x0409 => tramp_return_imm_0409, // LANG_EN_US
        0x1234 => tramp_return_imm_1234,
        0x5678 => tramp_return_imm_5678,
        437 => tramp_return_imm_437,   // GetOEMCP
        1252 => tramp_return_imm_1252, // GetACP
        12_345 => tramp_return_imm_12345,
        // Uncommon imm: still avoid Cranelift via a slow generic path.
        _ => tramp_return_imm32_generic,
    }
}

// SAFETY: each trampoline is only invoked with a live `JitCtx` from `run_compiled`.
// That invariant is stated once, on `ctx_mut` above; the `unsafe { ctx_mut(ctx) }`
// call in each body below is exactly that one claim, not 13 restatements of it.
// The remaining `unsafe` blocks below dereference *other* pointers
// (`inv_gen_ptr`, `chain_slots`, a chain target, the guest-memory helpers) and
// keep their own per-site SAFETY comments.

unsafe extern "C" fn tramp_ret(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::Ret);
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_zero(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnZero);
    ctx.gpr[0] = 0;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_identity_rcx(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::IdentityRcx);
    ctx.gpr[0] = ctx.gpr[1];
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_1(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(1));
    ctx.gpr[0] = 1;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_1234(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(0x1234));
    ctx.gpr[0] = 0x1234;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_5678(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(0x5678));
    ctx.gpr[0] = 0x5678;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_12345(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(12_345));
    ctx.gpr[0] = 12_345;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_0409(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(0x0409));
    ctx.gpr[0] = 0x0409;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_437(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(437));
    ctx.gpr[0] = 437;
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_return_imm_1252(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(1252));
    ctx.gpr[0] = 1252;
    guest_ret(ctx);
    chain_tail(ctx);
}

/// Fallback for rare imm32: re-decode guest code at entry RIP (ctx.rip before ret).
unsafe extern "C" fn tramp_return_imm32_generic(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::ReturnImm32(0));
    // Entry RIP was set by run_compiled; body is `b8 imm32 c3`.
    let entry = ctx.rip;
    let imm = tramp_load_u32(ctx, entry.wrapping_add(1));
    if ctx.fault != 0 {
        return;
    }
    ctx.gpr[0] = u64::from(imm);
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_get_last_error(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::GetLastError);
    // Resolve the LAST-ERROR slot against the engine's bound TEB page: a
    // worker's `gs:[0x68]` stub must read ITS TEB, not the primary's fixed VA.
    let last_error_va = ctx.gs_base.wrapping_add(TEB_LAST_ERROR_OFFSET);
    let v = tramp_load_u32(ctx, last_error_va);
    if ctx.fault != 0 {
        return;
    }
    ctx.gpr[0] = u64::from(v);
    guest_ret(ctx);
    chain_tail(ctx);
}

unsafe extern "C" fn tramp_set_last_error(ctx: *mut JitCtx) {
    let ctx = unsafe { ctx_mut(ctx) };
    enter_stub(ctx, MicroStub::SetLastError);
    let ecx = ctx.gpr[1] as u32;
    let last_error_va = ctx.gs_base.wrapping_add(TEB_LAST_ERROR_OFFSET);
    tramp_store_u32(ctx, last_error_va, ecx);
    if ctx.fault != 0 {
        return;
    }
    guest_ret(ctx);
    chain_tail(ctx);
}

#[inline]
fn mark_dirty(ctx: &mut JitCtx, mask: u16) {
    // When a micro-stub runs inside a chain (`chain_depth > 0`), the enclosing
    // Cranelift blocks have dirtied arbitrary GPRs without touching
    // `gpr_dirty_bits`. A partial mask would make the outermost writeback
    // (run_compiled) sync only the stub's regs back to the host regfile and
    // drop the chain's other register updates (e.g. a `lea r12` two blocks
    // earlier) — the next dispatched block then reloads a stale value.
    if ctx.chain_depth != 0 {
        ctx.gpr_dirty_bits = u64::from(ALL_DIRTY_BITS);
    } else {
        ctx.gpr_dirty_bits |= u64::from(mask);
    }
}

/// Book-keeping every micro-stub does on entry: dirty-mask the registers it
/// clobbers, and charge its retired guest instructions to the run-wide
/// accumulator the Cranelift blocks also fold into (`JitCtx::insn_acc`).
///
/// Without the charge a stub reached through a chain would contribute nothing
/// to `jit_insns`, re-introducing the static-per-entry undercount for
/// `GetLastError` / `SetLastError` — by far the hottest stubs in a call-heavy
/// guest.
#[inline]
fn enter_stub(ctx: &mut JitCtx, stub: MicroStub) {
    mark_dirty(ctx, stub.dirty_mask());
    ctx.insn_acc = ctx.insn_acc.saturating_add(u64::from(stub.insn_count()));
}

/// Pop guest return address, update RSP / shadow, set RIP.
fn guest_ret(ctx: &mut JitCtx) {
    if ctx.fault != 0 {
        return;
    }
    let rsp = ctx.gpr[4];
    let ret_va = tramp_load_u64(ctx, rsp);
    if ctx.fault != 0 {
        return;
    }
    ctx.gpr[4] = rsp.wrapping_add(8);
    shadow_pop_check(ctx, ret_va);
    ctx.rip = ret_va;
}

fn shadow_pop_check(ctx: &mut JitCtx, ret_va: u64) {
    let sp = ctx.shadow_sp;
    if sp == 0 {
        return;
    }
    let sp1 = sp.wrapping_sub(1);
    let idx = (sp1 as usize) & (SHADOW_DEPTH - 1);
    let predicted = ctx.shadow_ret[idx];
    if predicted == ret_va {
        ctx.shadow_sp = sp1;
    } else {
        ctx.shadow_sp = 0;
    }
}

/// Late-bound chain into the next Ready block (same host ABI as Cranelift).
fn chain_tail(ctx: &mut JitCtx) {
    if ctx.fault != 0 {
        return;
    }
    // Match Cranelift `emit_chain_or_exit` host-stack cap.
    if ctx.chain_depth >= MAX_CHAIN_DEPTH {
        return;
    }
    // Cross-thread invalidation guard — the Rust-side twin of the emitted
    // hop guard. A micro-stub inside a chain never re-enters the dispatcher
    // on its own, so a generation bump past this session's bake must decline
    // chaining here or stale native code keeps running across the edge.
    if JitConfig::get().chain_enabled() && !ctx.inv_gen_ptr.is_null() {
        // SAFETY: `inv_gen_ptr` targets `JitShared::invalidate_gen`, whose
        // `Arc` target lives for the process lifetime.
        let cur = unsafe { (*ctx.inv_gen_ptr).load(Ordering::Acquire) };
        if cur != ctx.inv_gen_baked {
            return;
        }
    }
    let fn_ptr = chain_lookup(ctx, ctx.rip);
    if fn_ptr == 0 {
        return;
    }
    // Successor may be a Cranelift block that dirties arbitrary GPRs without
    // updating `gpr_dirty_bits` — force full host writeback for this session.
    ctx.gpr_dirty_bits = u64::from(ALL_DIRTY_BITS);
    ctx.chain_depth = ctx.chain_depth.saturating_add(1);
    // SAFETY: pointer published by chain_table_insert from a finalized block,
    // so the transmuted `f` has this block's `extern "C" fn(*mut JitCtx)`
    // signature; and the call itself passes the `JitCtx` that `ctx_mut` proved
    // live for this frame, which `f` (a block or micro-stub) does not retain.
    let f: unsafe extern "C" fn(*mut JitCtx) =
        unsafe { std::mem::transmute(fn_ptr as usize as *const u8) };
    unsafe {
        f(ctx);
    }
    ctx.chain_depth = ctx.chain_depth.saturating_sub(1);
}

fn chain_lookup(ctx: &JitCtx, va: u64) -> u64 {
    if va == 0 || ctx.chain_slots.is_null() {
        return 0;
    }
    // SAFETY: `chain_slots` is a live [ChainSlot; CHAIN_SLOTS] pointer for this call.
    let slots = unsafe { std::slice::from_raw_parts(ctx.chain_slots, CHAIN_SLOTS) };
    let mut i = chain_hash(va);
    for _ in 0..16 {
        let s = slots[i];
        if s.va == va {
            return s.fn_ptr;
        }
        if s.va == 0 {
            return 0;
        }
        i = (i + 1) & (CHAIN_SLOTS - 1);
    }
    0
}

fn tramp_load_u64(ctx: &mut JitCtx, addr: u64) -> u64 {
    // SAFETY: ctx is live; load helper matches Cranelift host import.
    unsafe { wie_jit_load(std::ptr::from_mut(ctx), addr, 8, ctx.rip) }
}

fn tramp_load_u32(ctx: &mut JitCtx, addr: u64) -> u32 {
    let v = unsafe { wie_jit_load(std::ptr::from_mut(ctx), addr, 4, ctx.rip) };
    v as u32
}

fn tramp_store_u32(ctx: &mut JitCtx, addr: u64, value: u32) {
    // SAFETY: ctx is live (borrowed from the trampoline frame); store helper
    // matches the Cranelift host import and records any fault in `ctx`.
    unsafe {
        wie_jit_store(std::ptr::from_mut(ctx), addr, 4, u64::from(value), ctx.rip);
    }
}
