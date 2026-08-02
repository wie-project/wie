//! TLB hot-path helpers: software-permission checks, pin resolution, multi-way
//! //! set-associative lookups, and the single-page host-pointer resolver (`tlb_page_ptr`).

use super::super::config::JitConfig;
use super::{
    GuestVa, JitCtx, MemPin, STICKY_WAYS, TLB_EMPTY, TLB_PROT_R, TLB_PROT_W, TLB_WAYS_PER_SET,
    TlbBucket, TlbBucketAux, tlb_set_index,
};

use crate::mem::PAGE_SIZE;

pub(super) fn set_fault(ctx: &mut JitCtx, insn_ip: u64, addr: u64, size: u64, access: u64) {
    ctx.fault = 1;
    ctx.rip = insn_ip;
    ctx.fault_addr = addr;
    ctx.fault_size = size;
    ctx.fault_access = access;
}

/// Promote a mapped page into multi sticky (IR) + last-hit mirror + multi-way cache warm.
///
/// Sticky ways are **MRU-ordered**: way 0 is always the last hit so sequential
/// streams take one IR probe; thrash of ≤[`STICKY_WAYS`] pages stays in IR.
pub(super) fn tlb_set_hot(
    ctx: &mut JitCtx,
    page_key: u64,
    page_base: *mut u8,
    prot: u8,
    generation: u64,
) {
    if ctx.tlb_hot_page != page_key && ctx.tlb_hot_page != TLB_EMPTY && !ctx.tlb_hot_ptr.is_null() {
        ctx.mem_path.sticky_swaps = ctx.mem_path.sticky_swaps.saturating_add(1);
    }
    // Last-hit mirror (helper + diagnostics).
    ctx.tlb_hot_page = page_key;
    ctx.tlb_hot_ptr = page_base;
    ctx.tlb_hot_prot = u64::from(prot);
    ctx.tlb_hot_gen = generation;

    // Find existing way (if any).
    let mut found = None;
    for w in 0..STICKY_WAYS {
        if ctx.sticky_page.get(w).copied() == Some(page_key) {
            found = Some(w);
            break;
        }
    }
    let prot_u = u64::from(prot);
    if let Some(0) = found {
        // Already MRU — refresh metadata only.
        if let Some(p) = ctx.sticky_ptr.get_mut(0) {
            *p = page_base;
        }
        if let Some(p) = ctx.sticky_prot.get_mut(0) {
            *p = prot_u;
        }
        if let Some(g) = ctx.sticky_gen.get_mut(0) {
            *g = generation;
        }
        return;
    }
    // Build new MRU list: [new, …previous without new…]
    let mut pages = [TLB_EMPTY; STICKY_WAYS];
    let mut ptrs = [std::ptr::null_mut(); STICKY_WAYS];
    let mut prots = [0_u64; STICKY_WAYS];
    let mut gens = [0_u64; STICKY_WAYS];
    pages[0] = page_key;
    ptrs[0] = page_base;
    prots[0] = prot_u;
    gens[0] = generation;
    let mut dst = 1_usize;
    for w in 0..STICKY_WAYS {
        if Some(w) == found {
            continue; // drop old slot; reinserted at 0
        }
        if dst >= STICKY_WAYS {
            break;
        }
        let pk = ctx.sticky_page.get(w).copied().unwrap_or(TLB_EMPTY);
        if pk == TLB_EMPTY {
            continue;
        }
        pages[dst] = pk;
        ptrs[dst] = ctx
            .sticky_ptr
            .get(w)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        prots[dst] = ctx.sticky_prot.get(w).copied().unwrap_or(0);
        gens[dst] = ctx.sticky_gen.get(w).copied().unwrap_or(0);
        dst = dst.saturating_add(1);
    }
    ctx.sticky_page = pages;
    ctx.sticky_ptr = ptrs;
    ctx.sticky_prot = prots;
    ctx.sticky_gen = gens;
    // sticky_rr unused with MRU; keep field for ABI stability / future policy.
}

/// Classify guest VA against filled pins (diagnostic only).
///
/// Slot 0 = stack; slots 1.. = process heap + VirtualAlloc data pins.
pub(super) fn classify_addr_vs_pins(ctx: &mut JitCtx, addr: u64, size: usize) {
    let va = GuestVa::new(addr);
    let end = GuestVa::new(addr.saturating_add(u64::try_from(size).unwrap_or(0)));
    let mut in_stack = false;
    let mut in_data = false;
    for (i, pin) in ctx.pins.iter().enumerate() {
        if pin.contains(va, end) {
            if i == 0 {
                in_stack = true;
            } else {
                in_data = true;
            }
        }
    }
    if in_stack {
        ctx.mem_path.addr_in_stack_pin = ctx.mem_path.addr_in_stack_pin.saturating_add(1);
    } else if in_data {
        ctx.mem_path.addr_in_heap_pin = ctx.mem_path.addr_in_heap_pin.saturating_add(1);
    } else {
        ctx.mem_path.addr_outside_pins = ctx.mem_path.addr_outside_pins.saturating_add(1);
    }
}

/// Why multi sticky would miss (first failing predicate; exclusive buckets).
pub(super) fn classify_sticky_miss(ctx: &mut JitCtx, page_key: u64, write: bool) {
    let cur_gen = ctx.mem_gen;
    let mut saw_key = false;
    let mut saw_gen = false;
    for w in 0..STICKY_WAYS {
        if ctx.sticky_page.get(w).copied() != Some(page_key) {
            continue;
        }
        saw_key = true;
        let host = ctx
            .sticky_ptr
            .get(w)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        if host.is_null() {
            continue;
        }
        if ctx.sticky_gen.get(w).copied() != Some(cur_gen) {
            saw_gen = true;
            continue;
        }
        let prot = u8::try_from(ctx.sticky_prot.get(w).copied().unwrap_or(0)).unwrap_or(0);
        if !tlb_prot_allows(prot, write) {
            ctx.mem_path.sticky_miss_prot = ctx.mem_path.sticky_miss_prot.saturating_add(1);
            return;
        }
        // Would have hit — should not reach classify after a real sticky miss.
        return;
    }
    if saw_gen {
        ctx.mem_path.sticky_miss_gen = ctx.mem_path.sticky_miss_gen.saturating_add(1);
    } else {
        // No matching way (or null host) → key thrash / cold.
        let _ = saw_key;
        ctx.mem_path.sticky_miss_key = ctx.mem_path.sticky_miss_key.saturating_add(1);
    }
}

#[inline]
pub(super) fn tlb_prot_allows(prot: u8, write: bool) -> bool {
    if write {
        (u64::from(prot) & TLB_PROT_W) != 0
    } else {
        (u64::from(prot) & TLB_PROT_R) != 0
    }
}

pub(super) fn pack_tlb_prot(allow_r: bool, allow_w: bool) -> u8 {
    let mut p = 0_u8;
    if allow_r {
        p |= u8::try_from(TLB_PROT_R).unwrap_or(1);
    }
    if allow_w {
        p |= u8::try_from(TLB_PROT_W).unwrap_or(2);
    }
    p
}

/// Soft-translate via region pin when gen / bounds / R|W match.
///
/// On hit, also warms sticky + multi-way TLB so subsequent sticky IR can fire.
pub(super) fn pin_resolve(
    ctx: &mut JitCtx,
    addr: u64,
    size: usize,
    write: bool,
) -> Option<*mut u8> {
    let size_u = u64::try_from(size).unwrap_or(0);
    if size_u == 0 {
        return None;
    }
    let va = GuestVa::new(addr);
    let end = va.checked_add(size_u)?;
    let cur_gen = ctx.mem_gen;
    // Copy the matching pin out so the TLB can be mutated afterwards.
    // Previously a positional `(u64, u64, u64, u8)` tuple whose meaning lived
    // in a trailing comment — transposing guest_base and host_base there would
    // have compiled and silently translated into the wrong address space.
    let mut matched: Option<(MemPin, u8)> = None;
    for pin in &ctx.pins {
        if pin.is_empty() || pin.mem_gen != cur_gen {
            continue;
        }
        if !pin.contains(va, end) {
            continue;
        }
        let prot = u8::try_from(pin.allow).unwrap_or(0);
        if !tlb_prot_allows(prot, write) {
            continue;
        }
        matched = Some((*pin, prot));
        break;
    }
    let (pin, prot) = matched?;
    let pin_gen = pin.mem_gen;
    let host = pin.translate(va, end)?;
    let page_off = usize::try_from(addr & (PAGE_SIZE - 1)).unwrap_or(0);
    let page_key = addr >> 12;
    // SAFETY: host points into the pin span; subtract in-page offset for page base.
    let page_base = unsafe { host.sub(page_off) };
    tlb_install(ctx, page_key, page_base, prot, pin_gen);
    tlb_set_hot(ctx, page_key, page_base, prot, pin_gen);
    Some(host)
}

/// Install a page into the set-associative TLB (RR victim within set).
pub(super) fn tlb_install(
    ctx: &mut JitCtx,
    page_key: u64,
    page_base: *mut u8,
    prot: u8,
    generation: u64,
) {
    let set = tlb_set_index(page_key);
    let Some(bucket) = ctx.tlb_sets.get_mut(set) else {
        return;
    };
    let Some(aux) = ctx.tlb_aux.get_mut(set) else {
        return;
    };
    // Prefer empty / matching tag way.
    let mut way = None;
    for w in 0..TLB_WAYS_PER_SET {
        if bucket.tags.get(w).copied() == Some(page_key)
            || bucket.tags.get(w).copied() == Some(TLB_EMPTY)
        {
            way = Some(w);
            break;
        }
    }
    let way = way.unwrap_or_else(|| {
        let w = usize::from(aux.rr) & (TLB_WAYS_PER_SET - 1);
        aux.rr = aux.rr.wrapping_add(1);
        w
    });
    if let Some(t) = bucket.tags.get_mut(way) {
        *t = page_key;
    }
    if let Some(h) = bucket.host.get_mut(way) {
        *h = page_base;
    }
    if let Some(g) = aux.generation.get_mut(way) {
        *g = generation;
    }
    if let Some(p) = aux.prot.get_mut(way) {
        *p = prot;
    }
}

/// Scalar 4-way tag scan within a set.
pub(super) fn tlb_bucket_lookup_scalar(
    bucket: &TlbBucket,
    aux: &TlbBucketAux,
    page_key: u64,
    write: bool,
    cur_gen: u64,
) -> Option<(*mut u8, u8)> {
    for way in 0..TLB_WAYS_PER_SET {
        if bucket.tags.get(way).copied() != Some(page_key) {
            continue;
        }
        let host = bucket
            .host
            .get(way)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        if host.is_null() {
            continue;
        }
        if aux.generation.get(way).copied() != Some(cur_gen) {
            continue;
        }
        let prot = aux.prot.get(way).copied().unwrap_or(0);
        if !tlb_prot_allows(prot, write) {
            continue;
        }
        return Some((host, prot));
    }
    None
}

/// Neon / portable vector tag compare for one 4-way bucket.
pub(super) fn tlb_bucket_lookup(
    bucket: &TlbBucket,
    aux: &TlbBucketAux,
    page_key: u64,
    write: bool,
    cur_gen: u64,
) -> Option<(*mut u8, u8)> {
    if !JitConfig::get().tlb_neon_enabled() {
        return tlb_bucket_lookup_scalar(bucket, aux, page_key, write, cur_gen);
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `TlbBucket` is `align(16)`; tags are 4×u64 contiguous.
        let bits = unsafe { tlb_neon_tag_mask(bucket.tags.as_ptr(), page_key) };
        if bits == 0 {
            return None;
        }
        // First matching way (branchless prefer low index).
        let way = bits.trailing_zeros() as usize;
        if way >= TLB_WAYS_PER_SET {
            return None;
        }
        let host = bucket
            .host
            .get(way)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        if host.is_null() {
            return None;
        }
        if aux.generation.get(way).copied() != Some(cur_gen) {
            return None;
        }
        let prot = aux.prot.get(way).copied().unwrap_or(0);
        if !tlb_prot_allows(prot, write) {
            return None;
        }
        Some((host, prot))
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        tlb_bucket_lookup_scalar(bucket, aux, page_key, write, cur_gen)
    }
}

/// Compare 4 tags against `page_key` with two Neon `cmeq` ops; return way bitmask 0..15.
///
/// # Safety
/// `tags` must point to at least 4 `u64` values, 16-byte aligned.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
pub(super) unsafe fn tlb_neon_tag_mask(tags: *const u64, page_key: u64) -> u32 {
    use std::arch::aarch64::{vceqq_u64, vdupq_n_u64, vld1q_u64};
    // SAFETY: caller guarantees align + length; neon enabled on Apple Silicon.
    unsafe {
        let t01 = vld1q_u64(tags);
        let t23 = vld1q_u64(tags.add(2));
        let key = vdupq_n_u64(page_key);
        let m01 = vceqq_u64(t01, key);
        let m23 = vceqq_u64(t23, key);
        lane_eq_bit(m01, 0)
            | (lane_eq_bit(m01, 1) << 1)
            | (lane_eq_bit(m23, 0) << 2)
            | (lane_eq_bit(m23, 1) << 3)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
pub(super) fn lane_eq_bit(v: std::arch::aarch64::uint64x2_t, lane: usize) -> u32 {
    // vceqq lanes are all-ones or zero; extract via transmute to [u64; 2].
    // SAFETY: uint64x2_t is a 16-byte SIMD register; layout matches [u64; 2].
    let arr: [u64; 2] = unsafe { std::mem::transmute(v) };
    u32::from(arr.get(lane).copied().unwrap_or(0) != 0)
}

/// Resolve host page pointer via multi-way TLB (single-page accesses only).
///
/// Enforces software R/W bits and memory generation on every hit. Misses that
/// lack the required permission return `None` so the slow path can set a fault
/// via [`GuestMemory::read`] / [`GuestMemory::write`] (SPC oracle).
///
/// Updates [`JitCtx::mem_path`] counters: sticky IR already missed (caller is
/// the host helper), so each call is one helper invocation to classify.
pub(super) unsafe fn tlb_page_ptr(
    ctx: &mut JitCtx,
    addr: u64,
    size: usize,
    write: bool,
) -> Option<*mut u8> {
    classify_addr_vs_pins(ctx, addr, size);
    let page_off = usize::try_from(addr & (PAGE_SIZE - 1)).unwrap_or(0);
    let page_cap = usize::try_from(PAGE_SIZE).unwrap_or(0x1000);
    if page_off.saturating_add(size) > page_cap {
        ctx.mem_path.cross_page = ctx.mem_path.cross_page.saturating_add(1);
        return None; // cross-page → slow path
    }
    let page_key = addr >> 12; // PAGE_SIZE = 0x1000
    let cur_gen = ctx.mem_gen;
    // Multi sticky hit first (matches inline IR fast path).
    for w in 0..STICKY_WAYS {
        if ctx.sticky_page.get(w).copied() != Some(page_key) {
            continue;
        }
        let host = ctx
            .sticky_ptr
            .get(w)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        if host.is_null() {
            continue;
        }
        if ctx.sticky_gen.get(w).copied() != Some(cur_gen) {
            continue;
        }
        let prot = u8::try_from(ctx.sticky_prot.get(w).copied().unwrap_or(0)).unwrap_or(0);
        if !tlb_prot_allows(prot, write) {
            continue;
        }
        ctx.mem_path.sticky_hit = ctx.mem_path.sticky_hit.saturating_add(1);
        // Keep last-hit mirror coherent with the way that hit.
        ctx.tlb_hot_page = page_key;
        ctx.tlb_hot_ptr = host;
        ctx.tlb_hot_prot = u64::from(prot);
        ctx.tlb_hot_gen = cur_gen;
        // SAFETY: sticky ptr is a mapped page base; access stays in-page; SPC bits match.
        return Some(unsafe { host.add(page_off) });
    }
    classify_sticky_miss(ctx, page_key, write);
    let set = tlb_set_index(page_key);
    let hit = ctx
        .tlb_sets
        .get(set)
        .zip(ctx.tlb_aux.get(set))
        .and_then(|(bucket, aux)| tlb_bucket_lookup(bucket, aux, page_key, write, cur_gen));
    if let Some((page_base, prot)) = hit {
        ctx.mem_path.multi_hit = ctx.mem_path.multi_hit.saturating_add(1);
        tlb_set_hot(ctx, page_key, page_base, prot, cur_gen);
        // SAFETY: page mapped; access stays within the page.
        return Some(unsafe { page_base.add(page_off) });
    }
    // Region-direct pin (stack/heap arenas) before radix/page walk.
    if let Some(p) = pin_resolve(ctx, addr, size, write) {
        ctx.mem_path.pin_hit = ctx.mem_path.pin_hit.saturating_add(1);
        return Some(p);
    }
    // Miss: resolve via GuestMemory (committed + protect meta + host ptr).
    // SAFETY: `mem` set by `run_compiled` to the live guest map.
    let mem = unsafe { &*ctx.mem };
    let Some(entry) = mem
        .page_tlb_entry_walk(page_key)
        .or_else(|| mem.page_tlb_entry(page_key))
    else {
        ctx.mem_path.slow = ctx.mem_path.slow.saturating_add(1);
        return None;
    };
    // Install even if this access is denied so a later opposite access can hit;
    // but only return a pointer when the *current* access is allowed.
    let prot = pack_tlb_prot(entry.allow_r, entry.allow_w);
    tlb_install(ctx, page_key, entry.host, prot, entry.generation);
    tlb_set_hot(ctx, page_key, entry.host, prot, entry.generation);
    if !tlb_prot_allows(prot, write) {
        ctx.mem_path.slow = ctx.mem_path.slow.saturating_add(1);
        return None;
    }
    ctx.mem_path.walk_hit = ctx.mem_path.walk_hit.saturating_add(1);
    // SAFETY: page mapped; access stays within the page; permission checked.
    Some(unsafe { entry.host.add(page_off) })
}
