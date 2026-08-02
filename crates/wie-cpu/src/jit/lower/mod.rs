//! Lower pure-GPR (+ simple mem / jcc) blocks to Cranelift IR and finalize host code.
//!
//! Cast/index/arithmetic allows shared with other JIT modules live on `jit/mod.rs`.

#![allow(
    clippy::cast_possible_wrap, // mem width / offset → i32 for Cranelift
    clippy::many_single_char_names, // flag temps d/s/r in flags_* helpers
    clippy::too_many_arguments
)]

use super::JitEngine;
use super::block::{BlockTerm, DecodedInsn, analyze_block_stack_pin};
use super::config::JitConfig;
use super::fast_api::FastApiKind;
use crate::mem::GuestMemory;
use crate::regs::Rflags;
use cranelift::codegen::ir::{BlockArg, FuncRef, SigRef, UserFuncName};
use cranelift::prelude::*;
use cranelift_codegen::ir::{AliasRegionData, MemFlagsData};
use cranelift_module::{FuncId, Linkage, Module};
use iced_x86::{Instruction, Mnemonic, OpKind};
use std::borrow::Cow;
use std::collections::HashMap;

/// User-id for the "guest_data" alias region we install on every compiled function.
///
/// Stable so `AliasRegionSet::insert` deduplicates within a function; per-function
/// scope is enough because we do not enable Cranelift inlining.
pub(super) const GUEST_DATA_REGION_USER_ID: u32 = 1;

/// Set-associative TLB: number of sets (power of two). `SETS × WAYS` total entries.
pub(super) const TLB_SETS: usize = 16;
/// Ways per set (4-way; Neon tag compare loads two `I64X2` / `vld1q_u64`).
pub(super) const TLB_WAYS_PER_SET: usize = 4;
/// Empty TLB slot marker (`page_key == TLB_EMPTY`).
pub(super) const TLB_EMPTY: u64 = u64::MAX;

/// TLB / sticky software permission: bit0 = read, bit1 = write.
pub(super) const TLB_PROT_R: u64 = 1;
pub(super) const TLB_PROT_W: u64 = 2;

/// 4-way tag+host line (16-byte aligned for Neon loads).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub(super) struct TlbBucket {
    /// Guest page keys (`va >> 12`) for 4 ways.
    pub tags: [u64; TLB_WAYS_PER_SET],
    /// Host page bases (non-owning soft-translate pointers).
    pub host: [*mut u8; TLB_WAYS_PER_SET],
}

/// Per-set generation, prot bits, and RR victim.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub(super) struct TlbBucketAux {
    pub generation: [u64; TLB_WAYS_PER_SET],
    pub prot: [u8; TLB_WAYS_PER_SET],
    /// Next victim way within the set (0..3).
    pub rr: u8,
    pub _pad: [u8; 11],
}

/// Chain-table slot: guest VA → host fn ptr (0 = empty). AoS pair so that
/// linear probing loads both fields in one cache-line miss.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct ChainSlot {
    pub va: u64,
    pub fn_ptr: u64,
}

impl ChainSlot {
    #[inline]
    pub(super) const fn empty() -> Self {
        Self { va: 0, fn_ptr: 0 }
    }
}

/// Empty bucket constructor (const-friendly for array init).
#[must_use]
pub(super) const fn empty_tlb_bucket() -> TlbBucket {
    TlbBucket {
        tags: [TLB_EMPTY; TLB_WAYS_PER_SET],
        host: [std::ptr::null_mut(); TLB_WAYS_PER_SET],
    }
}

#[must_use]
pub(super) const fn empty_tlb_aux() -> TlbBucketAux {
    TlbBucketAux {
        generation: [0; TLB_WAYS_PER_SET],
        prot: [0; TLB_WAYS_PER_SET],
        rr: 0,
        _pad: [0; 11],
    }
}

/// One XMM slot as lo/hi u64 with 16-byte alignment (Neon-friendly bank).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct XmmSlot {
    pub lo: u64,
    pub hi: u64,
}

impl XmmSlot {
    pub(super) const ZERO: Self = Self { lo: 0, hi: 0 };

    #[must_use]
    pub(super) fn from_u128(v: u128) -> Self {
        Self {
            lo: v as u64,
            hi: (v >> 64) as u64,
        }
    }

    #[must_use]
    pub(super) fn to_u128(self) -> u128 {
        u128::from(self.lo) | (u128::from(self.hi) << 64)
    }
}

/// Set index for the 4-way TLB: XOR-fold high `page_key` bits into the low
/// `log2(TLB_SETS)` bits so allocations whose base VAs share the same low
/// bits (e.g. 64 KiB-aligned arenas — stack, heap, VirtualAlloc reserves) do
/// not all collide on the same set. `page_key = va >> 12`, so bits 0..3 of
/// `page_key` are va bits 12..15; a 64 KiB-aligned base has those zero and
/// would land in set 0 without folding.
#[inline]
pub(super) fn tlb_set_index(page_key: u64) -> usize {
    let mixed = page_key ^ (page_key >> 4) ^ (page_key >> 8) ^ (page_key >> 12);
    (mixed as usize) & (TLB_SETS - 1)
}

/// Open-addressing slots for guest-VA → host block fn (block chaining).
pub(super) const CHAIN_SLOTS: usize = 512;
/// Shadow return-stack depth (power of two; modular index).
pub(super) const SHADOW_DEPTH: usize = 32;
/// Region-direct pin slots (stack + primary heap). Phase 4.1.
/// Must match [`crate::mem::JIT_REGION_PIN_SLOTS`] (stack + heap + VA pins).
pub(super) const PIN_SLOTS: usize = crate::mem::JIT_REGION_PIN_SLOTS;
/// Multi sticky ways for inline IR (last-N pages before helper / multi-way TLB).
///
/// 2 balances 7za (large WS full-miss tax vs small-WS hit rate). 4 helps more
/// on tiny working sets but pays 4 probes on every thrash miss.
pub(super) const STICKY_WAYS: usize = 2;
/// Bytes per [`MemPin`] (`repr(C)`: 5×u64).
pub(super) const PIN_STRIDE: i32 = 40;
/// Monomorphic edge inline-cache slots (Phase 4.2 data-plane chaining).
pub(super) const EDGE_IC_SLOTS: usize = 4;

/// A guest virtual address.
///
/// WIE's core invariant is *guest VA ≠ host VA* — every guest access soft
/// translates through a region/arena base. [`MemPin`] is where both address
/// spaces meet, and as bare `u64` the only thing separating them was field
/// naming. `repr(transparent)` keeps the layout byte-identical to `u64`, so
/// the `repr(C)` struct below and the Cranelift IR that reads it at fixed
/// offsets are unaffected.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub(super) struct GuestVa(u64);

impl GuestVa {
    pub(super) const ZERO: Self = Self(0);

    #[inline]
    pub(super) const fn new(va: u64) -> Self {
        Self(va)
    }

    /// Exclusive end of `[self, self + len)`, or `None` on overflow.
    #[inline]
    pub(super) fn checked_add(self, len: u64) -> Option<Self> {
        self.0.checked_add(len).map(Self)
    }

    /// Byte distance from `base` to `self` (caller has ordered them).
    #[inline]
    pub(super) fn offset_from(self, base: Self) -> u64 {
        self.0.wrapping_sub(base.0)
    }
}

/// A host address — the integer form of a `*mut u8` into an mmap arena.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) struct HostAddr(u64);

impl HostAddr {
    pub(super) const NULL: Self = Self(0);

    #[inline]
    #[expect(clippy::as_conversions)] // pointer → integer for the repr(C) slot
    pub(super) fn from_ptr(p: *mut u8) -> Self {
        Self(p as u64)
    }

    #[inline]
    pub(super) const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Host pointer `self + off`.
    ///
    /// # Safety
    /// `off` must stay within the mapped arena this address came from.
    #[inline]
    #[expect(clippy::as_conversions)] // integer → pointer, inverse of `from_ptr`
    pub(super) unsafe fn add(self, off: usize) -> *mut u8 {
        unsafe { (self.0 as *mut u8).add(off) }
    }
}

/// Soft-translated region pin (stack / heap / VirtualAlloc) for Phase 4.1 JIT.
///
/// Empty pin: `host_base` null. Filled at each `run_compiled` from
/// [`crate::mem::GuestMemory::jit_region_pins`]; gen must match `mem_gen`.
///
/// The two address spaces are distinct types here so that
/// `host = host_base + (va - guest_base)` cannot be assembled from the wrong
/// operands — see [`Self::translate`], the single place that arithmetic lives.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct MemPin {
    /// Inclusive guest base VA.
    pub guest_base: GuestVa,
    /// Exclusive guest end VA.
    pub guest_end: GuestVa,
    /// Host soft-translate base.
    pub host_base: HostAddr,
    /// Memory generation at pin install.
    pub mem_gen: u64,
    /// Software R/W bits (`TLB_PROT_R` / `TLB_PROT_W`), intersection over range.
    pub allow: u64,
}

impl MemPin {
    /// Disabled / empty pin.
    pub(super) const EMPTY: Self = Self {
        guest_base: GuestVa::ZERO,
        guest_end: GuestVa::ZERO,
        host_base: HostAddr::NULL,
        mem_gen: 0,
        allow: 0,
    };

    /// Whether this slot carries no mapping.
    #[inline]
    pub(super) const fn is_empty(&self) -> bool {
        self.host_base.is_null()
    }

    /// Guest bytes covered by this pin (0 when empty).
    #[inline]
    pub(super) fn span_bytes(&self) -> u64 {
        self.guest_end.offset_from(self.guest_base)
    }

    /// Whether `[addr, addr+size)` lies entirely inside the pinned span.
    #[inline]
    pub(super) fn contains(&self, addr: GuestVa, end: GuestVa) -> bool {
        !self.is_empty() && addr >= self.guest_base && end <= self.guest_end
    }

    /// Soft translate `addr` to its host pointer.
    ///
    /// This is the *only* place `host = host_base + (va - guest_base)` is
    /// computed for a pin. Both operands are distinct types, so the guest and
    /// host bases cannot be swapped, and the containment check that makes the
    /// pointer arithmetic sound happens here rather than at each call site.
    #[inline]
    pub(super) fn translate(&self, addr: GuestVa, end: GuestVa) -> Option<*mut u8> {
        if !self.contains(addr, end) {
            return None;
        }
        let off = usize::try_from(addr.offset_from(self.guest_base)).ok()?;
        // SAFETY: `contains` proved `off` is within the pinned arena span, and
        // `host_base` is that span's soft-translate base.
        Some(unsafe { self.host_base.add(off) })
    }

    /// Build from a [`crate::mem::RegionPinInfo`] (or empty if `None`).
    pub(super) fn from_info(info: Option<crate::mem::RegionPinInfo>) -> Self {
        let Some(p) = info else {
            return Self::EMPTY;
        };
        if p.host_base.is_null() || p.guest_end <= p.guest_base {
            return Self::EMPTY;
        }
        let mut allow = 0_u64;
        if p.allow_r {
            allow |= TLB_PROT_R;
        }
        if p.allow_w {
            allow |= TLB_PROT_W;
        }
        if allow == 0 {
            return Self::EMPTY;
        }
        Self {
            guest_base: GuestVa::new(p.guest_base),
            guest_end: GuestVa::new(p.guest_end),
            host_base: HostAddr::from_ptr(p.host_base),
            mem_gen: p.generation,
            allow,
        }
    }
}

/// Guest register file snapshot for a compiled block (C ABI).
///
/// Layout is fixed; host mem helpers and Cranelift use the same offsets.
#[repr(C)]
pub(super) struct JitCtx {
    pub gpr: [u64; 16],
    pub rflags: u64,
    pub rip: u64,
    /// Guest memory for load/store host helpers (cross-page / fault).
    pub mem: *mut GuestMemory,
    /// Non-zero → invalid memory; `rip` holds faulting guest IP.
    pub fault: u64,
    pub fault_addr: u64,
    pub fault_size: u64,
    /// 0 = read, 1 = write (matches iced ACCESS_*).
    pub fault_access: u64,
    /// Set-associative multi-way page TLB (Phase 5.5 Track B).
    pub tlb_sets: [TlbBucket; TLB_SETS],
    /// Parallel gen/prot/rr for [`Self::tlb_sets`].
    pub tlb_aux: [TlbBucketAux; TLB_SETS],
    /// XMM0..XMM15 as 16-byte aligned slots (lo/hi layout for IR offsets).
    pub xmm: [XmmSlot; 16],
    /// Shadow return stack: push count (modular index via `sp & (SHADOW_DEPTH-1)`).
    pub shadow_sp: u64,
    /// Predicted guest return addresses for `call`/`ret` chaining.
    pub shadow_ret: [u64; SHADOW_DEPTH],
    /// Pointer to [`CHAIN_SLOTS`] `(va, fn_ptr)` pairs (owned by `JitCpu`,
    /// live for `run_compiled`). AoS layout — one 16-byte pair per probe
    /// stays inside a single cache line, halving L1 traffic vs. the previous
    /// parallel `chain_va` / `chain_fn` arrays that lived in separate lines.
    pub chain_slots: *mut ChainSlot,
    /// Sticky single-page TLB for inline IR mem (last hit/fill); `TLB_EMPTY` if cold.
    pub tlb_hot_page: u64,
    /// Host base pointer for [`Self::tlb_hot_page`] (page-aligned guest data).
    pub tlb_hot_ptr: *mut u8,
    /// Cumulative dirty GPR mask for host writeback (`bit i` → `gpr[i]` changed).
    /// Hand-written trampolines OR their bits; Cranelift leaves 0 → host syncs all 16.
    /// Set to `0xffff` before late-bound chain so a subsequent Cranelift block is covered.
    pub gpr_dirty_bits: u64,
    /// Phase 0: host load helper invocations during this `run_compiled` (appended; IR-stable).
    pub load_calls: u64,
    /// Phase 0: host store helper invocations during this `run_compiled`.
    pub store_calls: u64,
    /// Software R/W bits for sticky page (`TLB_PROT_R` / `TLB_PROT_W`).
    pub tlb_hot_prot: u64,
    /// [`GuestMemory::generation`] snapshot for this `run_compiled` (pin/TLB gen check).
    pub mem_gen: u64,
    /// Generation recorded for the sticky hot page.
    pub tlb_hot_gen: u64,
    /// Region-direct pins (stack / heap / VA); empty when `host_base == 0`.
    pub pins: [MemPin; PIN_SLOTS],
    /// Phase 4.2 monomorphic edge IC: guest target VA (0 = empty).
    ///
    /// Data-plane only — never patches finalized host code. Speeds late-bound
    /// chain hits when a block repeatedly transfers to the same successor.
    pub edge_ic_va: [u64; EDGE_IC_SLOTS],
    /// Parallel host fn pointers for [`Self::edge_ic_va`].
    pub edge_ic_fn: [u64; EDGE_IC_SLOTS],
    /// Round-robin victim for edge-IC install after a full chain-table hit.
    pub edge_ic_rr: u64,
    /// Dynamic XMM dirty mask written by compiled blocks (`bit i` → XMMi).
    pub xmm_dirty_bits: u64,
    /// Helper mem-path breakdown for this `run_compiled` (not used from Cranelift IR).
    pub mem_path: MemPathSlice,
    /// Multi sticky page keys (`TLB_EMPTY` = cold). Appended after IR-stable fields.
    pub sticky_page: [u64; STICKY_WAYS],
    /// Host page bases parallel to [`Self::sticky_page`].
    pub sticky_ptr: [*mut u8; STICKY_WAYS],
    /// Software R/W bits per sticky way.
    pub sticky_prot: [u64; STICKY_WAYS],
    /// Generation per sticky way.
    pub sticky_gen: [u64; STICKY_WAYS],
    /// Round-robin victim for sticky install.
    pub sticky_rr: u64,
    /// Host call-chain depth for block chaining (`emit_chain_or_exit`).
    ///
    /// Each chain hop is a host `call` into the next compiled block. Unbounded
    /// depth overflows the host stack on long guest call trees (7za large scans).
    /// When this hits [`MAX_CHAIN_DEPTH`], chaining returns to the Rust dispatcher
    /// with RIP already set so the next block re-enters without nesting.
    pub chain_depth: u64,
}

/// Per-`run_compiled` mem helper resolution counters (appended after IR-stable layout).
///
/// Classifies why the sticky IR path missed and how the helper resolved the access.
/// Cheap saturating adds; accumulate into [`super::JitStats`] after each block.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct MemPathSlice {
    /// Helper sticky hit (IR already missed — rare unless race/gen refresh).
    pub sticky_hit: u64,
    /// Multi-way set-assoc TLB hit after sticky miss.
    pub multi_hit: u64,
    /// Region pin soft-translate hit.
    pub pin_hit: u64,
    /// Full page-walk install hit.
    pub walk_hit: u64,
    /// Cross-page access (forces slow path).
    pub cross_page: u64,
    /// `tlb_page_ptr` returned `None` → GuestMemory::read/write.
    pub slow: u64,
    /// Sticky miss: page key / empty hot (working-set thrash).
    pub sticky_miss_key: u64,
    /// Sticky miss: `tlb_hot_gen != mem_gen`.
    pub sticky_miss_gen: u64,
    /// Sticky miss: R/W bit denied.
    pub sticky_miss_prot: u64,
    /// Times sticky hot page key was replaced.
    pub sticky_swaps: u64,
    /// Helper VA fell inside stack pin bounds (regardless of resolve path).
    pub addr_in_stack_pin: u64,
    /// Helper VA fell inside heap pin bounds.
    pub addr_in_heap_pin: u64,
    /// Helper VA outside both pins (VirtualAlloc / image / other).
    pub addr_outside_pins: u64,
}

// Byte offsets into [`JitCtx`] used from Cranelift IR (must match `repr(C)`).
// gpr[16] @ 0, rflags @ 128, rip @ 136, mem @ 144, fault @ 152, …
pub(super) const OFF_RFLAGS: i32 = std::mem::offset_of!(JitCtx, rflags) as i32;
pub(super) const OFF_RIP: i32 = std::mem::offset_of!(JitCtx, rip) as i32;
pub(super) const OFF_FAULT: i32 = std::mem::offset_of!(JitCtx, fault) as i32;
pub(super) const OFF_SHADOW_SP: i32 = std::mem::offset_of!(JitCtx, shadow_sp) as i32;
pub(super) const OFF_XMM: i32 = std::mem::offset_of!(JitCtx, xmm) as i32;
pub(super) const OFF_SHADOW_RET: i32 = OFF_SHADOW_SP + 8;
pub(super) const OFF_TLB_HOT_PAGE: i32 = std::mem::offset_of!(JitCtx, tlb_hot_page) as i32;
pub(super) const OFF_TLB_HOT_PTR: i32 = std::mem::offset_of!(JitCtx, tlb_hot_ptr) as i32;
pub(super) const OFF_TLB_HOT_PROT: i32 = std::mem::offset_of!(JitCtx, tlb_hot_prot) as i32;
pub(super) const OFF_MEM_GEN: i32 = std::mem::offset_of!(JitCtx, mem_gen) as i32;
pub(super) const OFF_TLB_HOT_GEN: i32 = std::mem::offset_of!(JitCtx, tlb_hot_gen) as i32;
pub(super) const OFF_PINS: i32 = std::mem::offset_of!(JitCtx, pins) as i32;
pub(super) const OFF_EDGE_IC_VA: i32 = std::mem::offset_of!(JitCtx, edge_ic_va) as i32;
pub(super) const OFF_EDGE_IC_FN: i32 = std::mem::offset_of!(JitCtx, edge_ic_fn) as i32;
pub(super) const OFF_EDGE_IC_RR: i32 = std::mem::offset_of!(JitCtx, edge_ic_rr) as i32;
pub(super) const OFF_XMM_DIRTY: i32 = std::mem::offset_of!(JitCtx, xmm_dirty_bits) as i32;
pub(super) const OFF_STICKY_PAGE: i32 = std::mem::offset_of!(JitCtx, sticky_page) as i32;
pub(super) const OFF_STICKY_PTR: i32 = std::mem::offset_of!(JitCtx, sticky_ptr) as i32;
pub(super) const OFF_STICKY_PROT: i32 = std::mem::offset_of!(JitCtx, sticky_prot) as i32;
pub(super) const OFF_STICKY_GEN: i32 = std::mem::offset_of!(JitCtx, sticky_gen) as i32;
pub(super) const OFF_CHAIN_DEPTH: i32 = std::mem::offset_of!(JitCtx, chain_depth) as i32;

/// Max nested host frames for JIT block chaining.
///
/// Guest `call`/`jmp`/`ret` chain via host C `call` into the successor block.
/// ~48 keeps most hot chains in-process while staying well under default host
/// stacks even with large Cranelift frames (seen: stack overflow on 7za scan).
pub(super) const MAX_CHAIN_DEPTH: u64 = 48;

// Layout sanity: Cranelift IR offsets must match `repr(C)` packing.
const _: () = {
    assert!(std::mem::offset_of!(JitCtx, rflags) as i32 == OFF_RFLAGS);
    assert!(std::mem::offset_of!(JitCtx, rip) as i32 == OFF_RIP);
    assert!(std::mem::offset_of!(JitCtx, fault) as i32 == OFF_FAULT);
    assert!(std::mem::offset_of!(JitCtx, xmm) as i32 == OFF_XMM);
    assert!(std::mem::offset_of!(JitCtx, shadow_sp) as i32 == OFF_SHADOW_SP);
    assert!(std::mem::offset_of!(JitCtx, shadow_ret) as i32 == OFF_SHADOW_RET);
    assert!(std::mem::offset_of!(JitCtx, tlb_hot_page) as i32 == OFF_TLB_HOT_PAGE);
    assert!(std::mem::offset_of!(JitCtx, tlb_hot_ptr) as i32 == OFF_TLB_HOT_PTR);
    assert!(std::mem::offset_of!(JitCtx, tlb_hot_prot) as i32 == OFF_TLB_HOT_PROT);
    assert!(std::mem::offset_of!(JitCtx, mem_gen) as i32 == OFF_MEM_GEN);
    assert!(std::mem::offset_of!(JitCtx, tlb_hot_gen) as i32 == OFF_TLB_HOT_GEN);
    assert!(std::mem::offset_of!(JitCtx, pins) as i32 == OFF_PINS);
    assert!(std::mem::offset_of!(JitCtx, edge_ic_va) as i32 == OFF_EDGE_IC_VA);
    assert!(std::mem::offset_of!(JitCtx, edge_ic_fn) as i32 == OFF_EDGE_IC_FN);
    assert!(std::mem::offset_of!(JitCtx, edge_ic_rr) as i32 == OFF_EDGE_IC_RR);
    assert!(std::mem::offset_of!(JitCtx, xmm_dirty_bits) as i32 == OFF_XMM_DIRTY);
    assert!(std::mem::offset_of!(JitCtx, sticky_page) as i32 == OFF_STICKY_PAGE);
    assert!(std::mem::offset_of!(JitCtx, sticky_ptr) as i32 == OFF_STICKY_PTR);
    assert!(std::mem::offset_of!(JitCtx, sticky_prot) as i32 == OFF_STICKY_PROT);
    assert!(std::mem::offset_of!(JitCtx, sticky_gen) as i32 == OFF_STICKY_GEN);
    assert!(std::mem::offset_of!(JitCtx, chain_depth) as i32 == OFF_CHAIN_DEPTH);
    assert!(STICKY_WAYS > 0);
    assert!(std::mem::size_of::<MemPin>() == PIN_STRIDE as usize);
    assert!(std::mem::size_of::<XmmSlot>() == 16);
    assert!(std::mem::align_of::<XmmSlot>() >= 16);
    assert!(std::mem::align_of::<TlbBucket>() >= 16);
    assert!(std::mem::size_of::<TlbBucket>() == 64);
    assert!(TLB_SETS.is_power_of_two());
    assert!(SHADOW_DEPTH.is_power_of_two());
    assert!(CHAIN_SLOTS.is_power_of_two());
    assert!(EDGE_IC_SLOTS > 0);
};

/// Finalized block ready to run.
#[derive(Clone, Copy)]
pub(super) struct CompiledBlock {
    pub func: unsafe extern "C" fn(*mut JitCtx),
    /// Module function id (for block-chaining `declare_func_in_func`).
    /// `None` for hand-written trampolines (late-bound chain only).
    pub func_id: Option<FuncId>,
    pub insn_count: u32,
    /// Block touches XMM/SSE state — host must sync the XMM bank.
    /// Pure GPR blocks skip XMM copy on entry/exit (CPU + cache win).
    pub uses_sse: bool,
    /// Bit `i` set if XMMi is referenced (selective entry load).
    pub xmm_live_mask: u16,
    /// Bit `i` set if XMMi may be written (selective exit writeback).
    pub xmm_may_def_mask: u16,
    /// Guest code range covered by this block `[guest_start, guest_end)`.
    /// Used for range-selective cache invalidation on `mem_write`.
    pub guest_start: u64,
    pub guest_end: u64,
}

/// Hash a guest VA into a chain-table slot.
#[inline]
pub(super) fn chain_hash(va: u64) -> usize {
    let h = va.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h as usize) & (CHAIN_SLOTS - 1)
}

/// Insert or update a compiled block in the open-addressing chain table.
pub(super) fn chain_table_insert(chain_slots: &mut [ChainSlot], va: u64, fn_ptr: u64) {
    if va == 0 || fn_ptr == 0 {
        return;
    }
    let mut i = chain_hash(va);
    for _ in 0..CHAIN_SLOTS {
        let slot = chain_slots[i].va;
        if slot == 0 || slot == va {
            chain_slots[i].va = va;
            chain_slots[i].fn_ptr = fn_ptr;
            return;
        }
        i = (i + 1) & (CHAIN_SLOTS - 1);
    }
    // Table full: overwrite hashed slot.
    let i = chain_hash(va);
    chain_slots[i].va = va;
    chain_slots[i].fn_ptr = fn_ptr;
}

/// Clear all chain-table entries (cache invalidation).
pub(super) fn chain_table_clear(chain_slots: &mut [ChainSlot]) {
    for s in chain_slots.iter_mut() {
        *s = ChainSlot::empty();
    }
}

mod analysis;
mod emit;
mod flags;
mod gpr;
mod insn;
mod mem;
mod sse;
mod sse_fp;
mod string;
mod tlb;

use analysis::{
    analyze_def_xmm, analyze_live_gprs, analyze_live_xmm, block_has_fp, block_has_mem,
    block_has_string, block_needs_flags, load_xmm_pair, xmm_mask_from,
};
use emit::{
    MemEnv, SuperStack, emit_block_wide_stack_guard, emit_body_and_term, term_chain_targets,
};
use flags::{flag_bit, iconst_u64, mask_width};
use gpr::{
    bool_to_i64, effective_addr, flag_set, mark_dirty, op_width_bits, read_op_mem, reg_index,
    sext_to_i64, write_gpr, write_op_mem,
};
use insn::{PendingFlags, ShiftKind, flush_pending};
use mem::{call_load, call_store, hoist_pin_slot};

pub(super) use mem::{
    wie_jit_chain_lookup, wie_jit_host_span, wie_jit_load, wie_jit_store, wie_jit_string,
};
pub(super) use sse::{wie_sse_int_binop, wie_sse_pshufb_hi, wie_sse_pshufb_lo, wie_sse_shift};
pub(super) use sse_fp::{
    wie_f32_binop, wie_f64_binop, wie_sse_cvt, wie_sse_fp_binop, wie_sse_fp_unop,
};

pub(super) fn compile_block(
    eng: &mut JitEngine,
    start_rip: u64,
    insns: &[DecodedInsn],
    end_rip: u64,
    term: Option<BlockTerm>,
    call_fast: Option<FastApiKind>,
    chain: &HashMap<u64, FuncId>,
    bytes_len: u32,
) -> Result<CompiledBlock, String> {
    let live = analyze_live_gprs(insns);
    let live_xmm = analyze_live_xmm(insns);
    let def_xmm = analyze_def_xmm(insns);
    let xmm_live_mask = xmm_mask_from(&live_xmm);
    let xmm_may_def_mask = xmm_mask_from(&def_xmm);
    let needs_flags = block_needs_flags(insns, term);
    let has_fast_call = call_fast.is_some();
    let has_mem = block_has_mem(insns)
        || matches!(term, Some(BlockTerm::Call { .. } | BlockTerm::Ret))
        || has_fast_call;
    let has_sse = live_xmm.iter().any(|&x| x);
    let has_string = block_has_string(insns);
    let has_fp = block_has_fp(insns);
    let need_fp_helpers = has_fp && !JitConfig::get().simd_enabled();
    let need_host_span = has_string && JitConfig::get().string_inline_enabled();

    // Self-loop if jcc/jmp targets this block's entry (stay in native code).
    let self_loop = match term {
        Some(BlockTerm::Jmp { target }) if target == start_rip => true,
        Some(BlockTerm::Jcc {
            taken, not_taken, ..
        }) if taken == start_rip || not_taken == start_rip => true,
        _ => false,
    };

    let body: &[DecodedInsn];
    let term_insn: Option<&DecodedInsn>;
    if term.is_some() {
        let (t, b) = insns
            .split_last()
            .ok_or_else(|| "empty block with term".to_string())?;
        body = b;
        term_insn = Some(t);
    } else {
        body = insns;
        term_insn = None;
    }

    let name_id = eng.next_name;
    eng.next_name = eng.next_name.saturating_add(1);
    let name = format!("b{name_id}");

    let func_id = eng
        .module
        .declare_function(&name, Linkage::Local, &eng.block_sig)
        .map_err(|e| e.to_string())?;

    eng.ctx.func.signature = eng.block_sig.clone();
    eng.ctx.func.name = UserFuncName::user(0, func_id.as_u32());

    {
        let mut bcx = FunctionBuilder::new(&mut eng.ctx.func, &mut eng.func_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        // Cranelift forbids CFG edges into the function entry block (remove_constant_phis
        // asserts `edge.block != entry_block`). Self-loops therefore re-enter a dedicated
        // header block, never `entry`.
        bcx.seal_block(entry);

        let ctx_ptr = bcx.block_params(entry)[0];
        let flags = MemFlagsData::trusted();
        // Guest data accesses (through pin bias / sticky ptr / super stack /
        // host_span I8X16) go through a distinct alias region so Cranelift's
        // alias analysis can hoist JitCtx sticky/pin metadata loads across them.
        let guest_data_region = bcx.func.dfg.alias_regions.insert(AliasRegionData {
            user_id: GUEST_DATA_REGION_USER_ID,
            description: Cow::Borrowed("guest_data"),
        });
        let guest_flags = flags.with_alias_region(Some(guest_data_region));

        // Exit: gpr[16] + rflags as block params → store and return.
        // XMM is write-through to JitCtx (correct on mid-block mem faults).
        let exit = bcx.create_block();
        for _ in 0..16 {
            bcx.append_block_param(exit, types::I64);
        }
        bcx.append_block_param(exit, types::I64);

        // Host ABI signature for `call_indirect` late-bound chaining.
        let block_sig_ref: SigRef = bcx.import_signature(eng.block_sig.clone());

        let load_ref = if has_mem || has_sse {
            Some(eng.module.declare_func_in_func(eng.load_id, bcx.func))
        } else {
            None
        };
        let store_ref = if has_mem || has_sse {
            Some(eng.module.declare_func_in_func(eng.store_id, bcx.func))
        } else {
            None
        };
        let string_ref = if has_string {
            Some(eng.module.declare_func_in_func(eng.string_id, bcx.func))
        } else {
            None
        };
        let host_span_ref = if need_host_span {
            Some(eng.module.declare_func_in_func(eng.host_span_id, bcx.func))
        } else {
            None
        };
        let f32_ref = if need_fp_helpers {
            Some(eng.module.declare_func_in_func(eng.f32_id, bcx.func))
        } else {
            None
        };
        let f64_ref = if need_fp_helpers {
            Some(eng.module.declare_func_in_func(eng.f64_id, bcx.func))
        } else {
            None
        };
        // Packed-integer / FP / convert helpers: declared whenever the block
        // touches XMM state (unused imports are never materialized by Cranelift).
        let sse_int_ref = if has_sse {
            Some(eng.module.declare_func_in_func(eng.sse_int_id, bcx.func))
        } else {
            None
        };
        let sse_shift_ref = if has_sse {
            Some(eng.module.declare_func_in_func(eng.sse_shift_id, bcx.func))
        } else {
            None
        };
        let sse_pshufb_lo_ref = if has_sse {
            Some(
                eng.module
                    .declare_func_in_func(eng.sse_pshufb_lo_id, bcx.func),
            )
        } else {
            None
        };
        let sse_pshufb_hi_ref = if has_sse {
            Some(
                eng.module
                    .declare_func_in_func(eng.sse_pshufb_hi_id, bcx.func),
            )
        } else {
            None
        };
        let sse_fp_unop_ref = if has_sse {
            Some(
                eng.module
                    .declare_func_in_func(eng.sse_fp_unop_id, bcx.func),
            )
        } else {
            None
        };
        let sse_fp_binop_ref = if has_sse {
            Some(
                eng.module
                    .declare_func_in_func(eng.sse_fp_binop_id, bcx.func),
            )
        } else {
            None
        };
        let sse_cvt_ref = if has_sse {
            Some(eng.module.declare_func_in_func(eng.sse_cvt_id, bcx.func))
        } else {
            None
        };
        // Dynamic chain lookup (late-bound successors + ret targets).
        let lookup_ref = eng.module.declare_func_in_func(eng.lookup_id, bcx.func);

        // Declare UCRT imports used by this block.
        let mut ucrt_refs: [Option<FuncRef>; 7] = [None; 7];
        if let Some(kind) = call_fast {
            let id = eng.ucrt.for_kind(kind);
            ucrt_refs[kind as usize] = Some(eng.module.declare_func_in_func(id, bcx.func));
        }
        // Chain successors (already compiled blocks) — direct call when known.
        let mut chain_refs: HashMap<u64, FuncRef> = HashMap::new();
        if let Some(t) = term {
            for va in term_chain_targets(t) {
                if va == start_rip {
                    continue; // self-loop uses IR jump
                }
                if let Some(&fid) = chain.get(&va) {
                    chain_refs.insert(va, eng.module.declare_func_in_func(fid, bcx.func));
                }
            }
            // Also pre-declare return_ip for Call (common after callee returns).
            if let BlockTerm::Call { return_ip, .. } = t
                && return_ip != start_rip
                && let Some(&fid) = chain.get(&return_ip)
            {
                chain_refs.insert(return_ip, eng.module.declare_func_in_func(fid, bcx.func));
            }
        }
        // Fallthrough chain.
        if term.is_none()
            && let Some(&fid) = chain.get(&end_rip)
        {
            chain_refs.insert(end_rip, eng.module.declare_func_in_func(fid, bcx.func));
        }

        // Live GPRs: only load what the block uses. Self-loops keep a full set in
        // block params so back-edges pass SSA values (no JitCtx store/reload).
        let mut live_eff = live;
        if has_fast_call {
            live_eff[0] = true; // RAX result
            live_eff[1] = true; // RCX
            live_eff[2] = true; // RDX
            live_eff[8] = true; // R8
            live_eff[9] = true; // R9
        }
        if self_loop {
            live_eff.fill(true);
        }

        let mut gpr_vals = [bcx.ins().iconst(types::I64, 0); 16];
        let mut gpr_loaded = [false; 16];
        // Dirty = written this block; only dirty+loaded regs are flushed to JitCtx
        // on chain/exit (reg-mapping: avoid storing read-only live-ins).
        let mut gpr_dirty = [false; 16];
        let rflags_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RFLAGS));
        // Carry flags across self-loop iterations via a block param when needed.
        let pass_flags = needs_flags || self_loop;

        // Hoist pin descriptors on the **entry** block (once per run_compiled).
        // Slot 0 = stack. Slots 1.. = size-ranked data (heap + VirtualAlloc).
        // Data-pin IR is opt-in via `WIE_JIT_MEM=pin` (see hoist below). Default
        // sticky keeps stack + sticky only; helpers still `pin_resolve` all 8
        // slots (VA/heaps) so walks collapse without IR cascade tax on 7za.
        // Set `WIE_JIT_MEM=pin` to also probe top-2 data pins after sticky.
        let (stack_pin, data_pins) = if JitConfig::get().mem_inline_enabled() {
            let stack = Some(hoist_pin_slot(&mut bcx, ctx_ptr, flags, 0));
            // Default sticky: no data-pin IR (helpers cover VA via pin_resolve).
            // `WIE_JIT_MEM=pin`: top-2 size-ranked data pins after sticky.
            let mut data = Vec::new();
            if JitConfig::get().mem_pin_enabled() {
                const IR_DATA_PIN_SLOTS: usize = 2;
                let end = 1_usize.saturating_add(IR_DATA_PIN_SLOTS).min(PIN_SLOTS);
                data.reserve(IR_DATA_PIN_SLOTS);
                for slot in 1..end {
                    data.push(hoist_pin_slot(&mut bcx, ctx_ptr, flags, slot));
                }
            }
            (stack, data)
        } else {
            (None, Vec::new())
        };

        // Pre-compile scan: displacement range for block-wide stack pin guard.
        let stack_plan = if JitConfig::get().mem_inline_enabled() {
            analyze_block_stack_pin(body, term_insn)
        } else {
            None
        };
        // Base register must be live for the guard even if not otherwise used.
        if let Some(p) = stack_plan {
            live_eff[p.base_idx] = true;
        }

        // Entry: load GPRs / flags once (shared by super and normal paths).
        let mut entry_gpr = [bcx.ins().iconst(types::I64, 0); 16];
        let mut entry_loaded = [false; 16];
        for i in 0..16 {
            if live_eff[i] {
                let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
                let p = bcx.ins().iadd_imm(ctx_ptr, off);
                entry_gpr[i] = bcx.ins().load(types::I64, flags, p, 0);
                entry_loaded[i] = true;
            }
        }
        let entry_rflags = if pass_flags {
            bcx.ins().load(types::I64, flags, rflags_ptr, 0)
        } else {
            bcx.ins().iconst(types::I64, 0)
        };

        // Dual path when the block is stack-pin-shaped: super (bare host mem) vs
        // normal (sticky/pin probes). One block-wide guard on entry.
        // Default `WIE_JIT_SUPER` = self-loops only; `=all` opt-in; `=0` off.
        let dual_super = stack_plan.is_some()
            && stack_pin.is_some()
            && !has_string
            && call_fast.is_none()
            && JitConfig::get().super_enabled(self_loop);

        let mut headers_to_seal: Vec<Block> = Vec::new();
        // Tracks gpr_loaded for the exit store mask (union of paths; full when dual).
        let mut exit_gpr_loaded = entry_loaded;

        if dual_super && let (Some(plan), Some(spin)) = (stack_plan, stack_pin) {
            let base = entry_gpr[plan.base_idx];
            let guard = emit_block_wide_stack_guard(&mut bcx, &spin, base, &plan);
            let bias = bcx.ins().isub(spin.host_base, spin.guest_base);

            let super_blk = bcx.create_block();
            let normal_blk = bcx.create_block();
            bcx.ins().brif(guard, super_blk, &[], normal_blk, &[]);

            for (is_super, start_blk) in [(true, super_blk), (false, normal_blk)] {
                bcx.switch_to_block(start_blk);
                bcx.seal_block(start_blk);

                let mut path_gpr = entry_gpr;
                let mut path_loaded = entry_loaded;
                let mut path_dirty = [false; 16];
                let mut path_rflags = entry_rflags;

                let loop_header = if self_loop {
                    let h = bcx.create_block();
                    for &is_live in live_eff.iter().take(16) {
                        if is_live {
                            bcx.append_block_param(h, types::I64);
                        }
                    }
                    if pass_flags {
                        bcx.append_block_param(h, types::I64);
                    }
                    let mut args: Vec<BlockArg> = Vec::with_capacity(17);
                    for i in 0..16 {
                        if live_eff[i] {
                            args.push(BlockArg::Value(path_gpr[i]));
                        }
                    }
                    if pass_flags {
                        args.push(BlockArg::Value(path_rflags));
                    }
                    bcx.ins().jump(h, &args);
                    bcx.switch_to_block(h);
                    let params = bcx.block_params(h);
                    let mut pi = 0_usize;
                    for i in 0..16 {
                        if live_eff[i] {
                            path_gpr[i] = params[pi];
                            path_loaded[i] = true;
                            pi = pi.saturating_add(1);
                        }
                    }
                    if pass_flags {
                        path_rflags = params[params.len() - 1];
                    }
                    headers_to_seal.push(h);
                    h
                } else {
                    start_blk
                };

                let mut xmm_vals = [bcx.ins().iconst(types::I64, 0); 32];
                let mut xmm_loaded = [false; 16];
                for (i, &is_live) in live_xmm.iter().enumerate() {
                    if is_live {
                        load_xmm_pair(&mut bcx, ctx_ptr, flags, i, &mut xmm_vals, &mut xmm_loaded);
                    }
                }

                let mut mem_env = MemEnv {
                    ctx_ptr,
                    load_ref,
                    store_ref,
                    string_ref,
                    host_span_ref,
                    f32_ref,
                    f64_ref,
                    sse_int_ref,
                    sse_shift_ref,
                    sse_pshufb_lo_ref,
                    sse_pshufb_hi_ref,
                    sse_fp_unop_ref,
                    sse_fp_binop_ref,
                    sse_cvt_ref,
                    flags,
                    guest_flags,
                    exit,
                    ucrt_refs,
                    // Super path: no per-access probes. Normal: hoisted pins.
                    stack_pin: if is_super { None } else { Some(spin) },
                    data_pins: if is_super {
                        Vec::new()
                    } else {
                        data_pins.clone()
                    },
                    super_stack: if is_super {
                        Some(SuperStack { bias })
                    } else {
                        None
                    },
                };

                emit_body_and_term(
                    &mut bcx,
                    body,
                    term,
                    term_insn,
                    call_fast,
                    start_rip,
                    end_rip,
                    self_loop,
                    loop_header,
                    &live_eff,
                    pass_flags,
                    needs_flags,
                    ctx_ptr,
                    flags,
                    rflags_ptr,
                    exit,
                    &chain_refs,
                    lookup_ref,
                    block_sig_ref,
                    &mut path_gpr,
                    &mut path_loaded,
                    &mut path_dirty,
                    &mut path_rflags,
                    &mut xmm_vals,
                    &mut xmm_loaded,
                    &mut mem_env,
                )?;
                for i in 0..16 {
                    exit_gpr_loaded[i] |= path_loaded[i];
                }
            }
        } else {
            // Single path (no block-wide super, or ineligible shape).
            let loop_header = if self_loop {
                let h = bcx.create_block();
                for &is_live in live_eff.iter().take(16) {
                    if is_live {
                        bcx.append_block_param(h, types::I64);
                    }
                }
                if pass_flags {
                    bcx.append_block_param(h, types::I64);
                }
                let mut entry_args: Vec<BlockArg> = Vec::with_capacity(17);
                for i in 0..16 {
                    if live_eff[i] {
                        entry_args.push(BlockArg::Value(entry_gpr[i]));
                        gpr_vals[i] = entry_gpr[i];
                        gpr_loaded[i] = true;
                    }
                }
                if pass_flags {
                    entry_args.push(BlockArg::Value(entry_rflags));
                }
                bcx.ins().jump(h, &entry_args);
                bcx.switch_to_block(h);
                let params = bcx.block_params(h);
                let mut pi = 0_usize;
                for i in 0..16 {
                    if live_eff[i] {
                        gpr_vals[i] = params[pi];
                        gpr_loaded[i] = true;
                        pi = pi.saturating_add(1);
                    }
                }
                headers_to_seal.push(h);
                h
            } else {
                for i in 0..16 {
                    if entry_loaded[i] {
                        gpr_vals[i] = entry_gpr[i];
                        gpr_loaded[i] = true;
                    }
                }
                entry
            };

            let mut xmm_vals = [bcx.ins().iconst(types::I64, 0); 32];
            let mut xmm_loaded = [false; 16];
            for (i, &is_live) in live_xmm.iter().enumerate() {
                if is_live {
                    load_xmm_pair(&mut bcx, ctx_ptr, flags, i, &mut xmm_vals, &mut xmm_loaded);
                }
            }

            let mut rflags_val = if self_loop && pass_flags {
                let params = bcx.block_params(loop_header);
                params[params.len() - 1]
            } else if needs_flags {
                entry_rflags
            } else {
                bcx.ins().iconst(types::I64, 0)
            };

            let mut mem_env = MemEnv {
                ctx_ptr,
                load_ref,
                store_ref,
                string_ref,
                host_span_ref,
                f32_ref,
                f64_ref,
                sse_int_ref,
                sse_shift_ref,
                sse_pshufb_lo_ref,
                sse_pshufb_hi_ref,
                sse_fp_unop_ref,
                sse_fp_binop_ref,
                sse_cvt_ref,
                flags,
                guest_flags,
                exit,
                ucrt_refs,
                stack_pin,
                data_pins,
                super_stack: None,
            };

            emit_body_and_term(
                &mut bcx,
                body,
                term,
                term_insn,
                call_fast,
                start_rip,
                end_rip,
                self_loop,
                loop_header,
                &live_eff,
                pass_flags,
                needs_flags,
                ctx_ptr,
                flags,
                rflags_ptr,
                exit,
                &chain_refs,
                lookup_ref,
                block_sig_ref,
                &mut gpr_vals,
                &mut gpr_loaded,
                &mut gpr_dirty,
                &mut rflags_val,
                &mut xmm_vals,
                &mut xmm_loaded,
                &mut mem_env,
            )?;
            exit_gpr_loaded = gpr_loaded;
        }

        bcx.switch_to_block(exit);
        bcx.seal_block(exit);
        let (exit_gpr, exit_rflags) = {
            let exit_params = bcx.block_params(exit);
            let mut g = [exit_params[0]; 16];
            g.copy_from_slice(&exit_params[..16]);
            (g, exit_params[16])
        };
        for i in 0..16 {
            // Fault/exit path: store only GPRs that were actually loaded/defined.
            // Self-loops load the full set at entry (`live_eff.fill(true)`).
            //
            // Important: do **not** force all-16 stores for dual_super. Non-loop
            // super blocks only load live-ins; unloaded slots are SSA `iconst 0`.
            // Writing them back zeroed callee-saved regs (R12–R15, RBX, RBP, …)
            // and corrupted guests such as 7za (`i` → null base → VA 0x1000).
            if exit_gpr_loaded[i] || self_loop {
                let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
                let p = bcx.ins().iadd_imm(ctx_ptr, off);
                bcx.ins().store(flags, exit_gpr[i], p, 0);
            }
        }
        if needs_flags || self_loop {
            bcx.ins().store(flags, exit_rflags, rflags_ptr, 0);
        }
        bcx.ins().return_(&[]);
        for h in headers_to_seal {
            bcx.seal_block(h);
        }
        bcx.seal_all_blocks();
        bcx.finalize();
    }

    eng.module
        .define_function(func_id, &mut eng.ctx)
        .map_err(|e| e.to_string())?;
    eng.module.clear_context(&mut eng.ctx);
    eng.module
        .finalize_definitions()
        .map_err(|e| e.to_string())?;

    let code = eng.module.get_finalized_function(func_id);
    let func = unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut JitCtx)>(code) };

    let guest_end = start_rip.saturating_add(u64::from(bytes_len));
    Ok(CompiledBlock {
        func,
        func_id: Some(func_id),
        insn_count: u32::try_from(insns.len()).unwrap_or(0),
        uses_sse: has_sse || has_fp,
        xmm_live_mask,
        xmm_may_def_mask,
        guest_start: start_rip,
        guest_end,
    })
}

#[derive(Clone, Copy)]
pub(super) enum SseBit {
    Xor,
    And,
    Or,
    Andn,
}

pub(super) fn lower_term(
    bcx: &mut FunctionBuilder<'_>,
    term: BlockTerm,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    term_ip: u64,
) -> Result<Value, String> {
    match term {
        BlockTerm::Jmp { target } => Ok(iconst_u64(bcx, target)),
        BlockTerm::Jcc {
            mnemonic,
            taken,
            not_taken,
        } => {
            // Flags already flushed before terminator when pending/jcc.
            let cond = flag_cond(bcx, rflags, mnemonic)?;
            let t = iconst_u64(bcx, taken);
            let n = iconst_u64(bcx, not_taken);
            Ok(bcx.ins().select(cond, t, n))
        }
        BlockTerm::Call { target, return_ip } => {
            // push return_ip; exit at target (RSP updated).
            let ret = iconst_u64(bcx, return_ip);
            let rsp = gpr[4];
            let new_rsp = bcx.ins().iadd_imm(rsp, -8);
            call_store(bcx, mem, gpr, rflags, new_rsp, 8, ret, term_ip)?;
            gpr[4] = new_rsp;
            mark_dirty(dirty, 4);
            Ok(iconst_u64(bcx, target))
        }
        BlockTerm::Ret => {
            let rsp = gpr[4];
            let ret = call_load(bcx, mem, gpr, rflags, rsp, 8, term_ip)?;
            gpr[4] = bcx.ins().iadd_imm(rsp, 8);
            mark_dirty(dirty, 4);
            Ok(ret)
        }
    }
}

/// Map jcc / cmov / setcc mnemonics to shared condition evaluation.
pub(super) fn flag_cond(
    bcx: &mut FunctionBuilder<'_>,
    rflags: Value,
    m: Mnemonic,
) -> Result<Value, String> {
    let zf = flag_set(bcx, rflags, Rflags::ZF);
    let cf = flag_set(bcx, rflags, Rflags::CF);
    let sf = flag_set(bcx, rflags, Rflags::SF);
    let of = flag_set(bcx, rflags, Rflags::OF);
    let pf = flag_set(bcx, rflags, Rflags::PF);
    let zf1 = bool_to_i64(bcx, zf);
    let cf1 = bool_to_i64(bcx, cf);
    let sf1 = bool_to_i64(bcx, sf);
    let of1 = bool_to_i64(bcx, of);
    let pf1 = bool_to_i64(bcx, pf);
    let zero = iconst_u64(bcx, 0);
    let one = iconst_u64(bcx, 1);
    let not_zf = bcx.ins().bxor(zf1, one);
    let not_cf = bcx.ins().bxor(cf1, one);
    let not_of = bcx.ins().bxor(of1, one);
    let not_sf = bcx.ins().bxor(sf1, one);
    let not_pf = bcx.ins().bxor(pf1, one);

    let cond_i64 = match m {
        Mnemonic::Je | Mnemonic::Cmove | Mnemonic::Sete => zf1,
        Mnemonic::Jne | Mnemonic::Cmovne | Mnemonic::Setne => not_zf,
        Mnemonic::Ja | Mnemonic::Cmova | Mnemonic::Seta => bcx.ins().band(not_cf, not_zf),
        Mnemonic::Jae | Mnemonic::Cmovae | Mnemonic::Setae => not_cf,
        Mnemonic::Jb | Mnemonic::Cmovb | Mnemonic::Setb => cf1,
        Mnemonic::Jbe | Mnemonic::Cmovbe | Mnemonic::Setbe => bcx.ins().bor(cf1, zf1),
        Mnemonic::Jg | Mnemonic::Cmovg | Mnemonic::Setg => {
            let eq = bcx.ins().icmp(IntCC::Equal, sf1, of1);
            let eq1 = bool_to_i64(bcx, eq);
            bcx.ins().band(not_zf, eq1)
        }
        Mnemonic::Jge | Mnemonic::Cmovge | Mnemonic::Setge => {
            let eq = bcx.ins().icmp(IntCC::Equal, sf1, of1);
            bool_to_i64(bcx, eq)
        }
        Mnemonic::Jl | Mnemonic::Cmovl | Mnemonic::Setl => {
            let ne = bcx.ins().icmp(IntCC::NotEqual, sf1, of1);
            bool_to_i64(bcx, ne)
        }
        Mnemonic::Jle | Mnemonic::Cmovle | Mnemonic::Setle => {
            let ne = bcx.ins().icmp(IntCC::NotEqual, sf1, of1);
            let ne1 = bool_to_i64(bcx, ne);
            bcx.ins().bor(zf1, ne1)
        }
        Mnemonic::Jo | Mnemonic::Cmovo | Mnemonic::Seto => of1,
        Mnemonic::Jno | Mnemonic::Cmovno | Mnemonic::Setno => not_of,
        Mnemonic::Js | Mnemonic::Cmovs | Mnemonic::Sets => sf1,
        Mnemonic::Jns | Mnemonic::Cmovns | Mnemonic::Setns => not_sf,
        Mnemonic::Jp | Mnemonic::Cmovp | Mnemonic::Setp => pf1,
        Mnemonic::Jnp | Mnemonic::Cmovnp | Mnemonic::Setnp => not_pf,
        other => return Err(format!("cond {other:?}")),
    };
    Ok(bcx.ins().icmp(IntCC::NotEqual, cond_i64, zero))
}

pub(super) fn lower_cmov(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    m: Mnemonic,
) -> Result<(), String> {
    let cond = flag_cond(bcx, rflags, m)?;
    let src = read_op_mem(bcx, instr, 1, gpr, rflags, mem)?;
    let reg = instr.op_register(0);
    let idx = reg_index(reg)?;
    let old = gpr[idx];
    // Simulate taken write into a temp slot.
    let mut gpr_t = *gpr;
    let mut dirty_t = [false; 16];
    write_gpr(bcx, &mut gpr_t, &mut dirty_t, reg, src)?;
    gpr[idx] = bcx.ins().select(cond, gpr_t[idx], old);
    mark_dirty(dirty, idx);
    Ok(())
}

pub(super) fn lower_setcc(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    m: Mnemonic,
) -> Result<(), String> {
    let cond = flag_cond(bcx, rflags, m)?;
    let one = iconst_u64(bcx, 1);
    let zero = iconst_u64(bcx, 0);
    let val = bcx.ins().select(cond, one, zero);
    match instr.op0_kind() {
        OpKind::Register => write_gpr(bcx, gpr, dirty, instr.op_register(0), val),
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            call_store(bcx, mem, gpr, rflags, addr, 1, val, instr.ip())
        }
        _ => Err("setcc form".into()),
    }
}

/// Compute shift result now; pack flags lazily via [`PendingFlags::Shift`].
pub(super) fn lower_shift_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    kind: ShiftKind,
) -> Result<(), String> {
    // Prior ALU flags must be in `rflags` before we record a Shift pending
    // (shift flags are applied relative to current rflags for OF when count!=1).
    flush_pending(bcx, rflags, pending);

    let bits = op_width_bits(instr, 0)?;
    let dst_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let dst = mask_width(bcx, dst_raw, bits);
    let count_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let mask63 = iconst_u64(bcx, 0x3f);
    let count_masked = bcx.ins().band(count_raw, mask63);
    let bits_v = iconst_u64(bcx, u64::from(bits));
    let count_mod = if bits >= 64 {
        count_masked
    } else {
        bcx.ins().urem(count_masked, bits_v)
    };
    let is_zero = bcx.ins().icmp_imm(IntCC::Equal, count_mod, 0);

    let result_raw = match kind {
        ShiftKind::Shl => bcx.ins().ishl(dst, count_mod),
        ShiftKind::Shr => bcx.ins().ushr(dst, count_mod),
        ShiftKind::Sar => {
            let signed = sext_to_i64(bcx, dst, bits);
            bcx.ins().sshr(signed, count_mod)
        }
        ShiftKind::Rol => {
            let left = bcx.ins().ishl(dst, count_mod);
            let right_amt = bcx.ins().isub(bits_v, count_mod);
            let right = bcx.ins().ushr(dst, right_amt);
            bcx.ins().bor(left, right)
        }
        ShiftKind::Ror => {
            let right = bcx.ins().ushr(dst, count_mod);
            let left_amt = bcx.ins().isub(bits_v, count_mod);
            let left = bcx.ins().ishl(dst, left_amt);
            bcx.ins().bor(left, right)
        }
        ShiftKind::Rcl => {
            // Rcl: CF into LSB, shift left by count, MSB into CF
            let cf_val = flag_bit(bcx, *rflags, Rflags::CF);
            let left = bcx.ins().ishl(dst, count_mod);
            let right_amt = bcx.ins().isub(bits_v, count_mod);
            let right = bcx.ins().ushr(dst, right_amt);
            let cf_shift = bcx.ins().ishl(cf_val, right_amt);
            let tmp = bcx.ins().bor(left, right);
            bcx.ins().bor(tmp, cf_shift)
        }
        ShiftKind::Rcr => {
            // Rcr: CF into MSB, shift right by count
            let cf_val = flag_bit(bcx, *rflags, Rflags::CF);
            let right = bcx.ins().ushr(dst, count_mod);
            let left_amt = bcx.ins().isub(bits_v, count_mod);
            let left = bcx.ins().ishl(dst, left_amt);
            let cf_shift = bcx.ins().ishl(cf_val, left_amt);
            let tmp = bcx.ins().bor(left, right);
            bcx.ins().bor(tmp, cf_shift)
        }
    };
    let result = mask_width(bcx, result_raw, bits);
    let final_res = bcx.ins().select(is_zero, dst, result);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, final_res, bits)?;

    // Defer flag packing; materialize_shift_flags keeps old rflags when count_mod==0.
    *pending = PendingFlags::Shift {
        kind,
        dst,
        res: result,
        count_mod,
        bits,
    };
    Ok(())
}
