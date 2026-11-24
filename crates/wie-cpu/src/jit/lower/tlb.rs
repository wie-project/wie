//! TLB hot-path lookup helpers: software-permission checks, pin resolution,
//! multi-way set-associative lookups, installation into the direct-mapped
//! inline refill table, and the single-page host-pointer resolver
//! (`tlb_page_ptr`).

use super::super::config::JitConfig;
use super::super::gen_tlb::GenTlb;
use super::{
    GuestVa, JitCtx, MemPin, REFILL_SLOTS, RefillSlot, STICKY_WAYS, TLB_EMPTY, TLB_PROT_R,
    TLB_PROT_W, TLB_SETS, TLB_WAYS_PER_SET, TlbValue, refill_slot,
};

use crate::mem::PAGE_SIZE;

pub(super) fn set_fault(ctx: &mut JitCtx, insn_ip: u64, addr: u64, size: u64, access: u64) {
    ctx.fault = 1;
    ctx.rip = insn_ip;
    ctx.fault_addr = addr;
    ctx.fault_size = size;
    ctx.fault_access = access;
}

/// Install a resolved page into the **direct-mapped inline refill table** — the
/// table emitted code probes before it calls a helper.
///
/// Called from [`tlb_set_hot`], i.e. from all three resolution points of
/// [`tlb_page_ptr`] (bucket hit, pin hit, walk hit): whatever the helper just
/// resolved is what the next inline access hits. Populating only on a "clean"
/// hit would leave the slot cold after exactly the accesses that paid a helper.
///
/// `generation` is the generation the *page entry* was resolved under (not
/// necessarily the frame's `ctx.mem_gen`): a page resolved by a helper after a
/// mid-frame protect carries the newer generation and therefore misses the
/// inline probe until the dispatcher re-stamps `mem_gen`, which is the
/// conservative direction (helper, not a stale host pointer).
fn refill_install(ctx: &mut JitCtx, page_key: u64, page_base: *mut u8, prot: u8, generation: u64) {
    let slot = refill_slot(page_key);
    if let Some(s) = ctx.refill.get_mut(slot) {
        *s = RefillSlot::new(page_key ^ generation, page_base, u64::from(prot));
    }
}

/// Clear every refill slot (generation bump / thread switch / TLB invalidate).
///
/// The emitted probe's tag folds `mem_gen`, so a stale slot would already be
/// rejected; clearing makes the folded compare *exact* rather than merely
/// improbable — no installed slot can outlive the generation it was resolved
/// under, so `tag == page_key ^ mem_gen` reduces to `tag == page_key`. Costs one
/// 8 KiB memset on a map/protect event, next to the pin rebuild that event
/// already pays for.
pub(crate) fn refill_clear(ctx: &mut JitCtx) {
    ctx.refill = [RefillSlot::EMPTY; REFILL_SLOTS];
}

/// Promote a mapped page into multi sticky (host victim set) + last-hit mirror +
/// the inline refill slot.
///
/// Sticky ways are **MRU-ordered** host-side: way 0 is always the last hit.
/// Emitted IR no longer reads them (the inline probe is the refill table); they
/// are a victim set that [`tlb_page_ptr`] scans after an inline miss, which is
/// what keeps a thrashing page reachable without rebuilding the bucket.
pub(super) fn tlb_set_hot(
    ctx: &mut JitCtx,
    page_key: u64,
    page_base: *mut u8,
    prot: u8,
    generation: u64,
) {
    if ctx.mem_path_on != 0
        && ctx.tlb_hot_page != page_key
        && ctx.tlb_hot_page != TLB_EMPTY
        && !ctx.tlb_hot_ptr.is_null()
    {
        ctx.mem_path.sticky_swaps = ctx.mem_path.sticky_swaps.saturating_add(1);
    }
    // Last-hit mirror (helper + diagnostics).
    ctx.tlb_hot_page = page_key;
    ctx.tlb_hot_ptr = page_base;
    ctx.tlb_hot_prot = u64::from(prot);
    ctx.tlb_hot_gen = generation;
    // Inline fast path: the page the helper just resolved.
    refill_install(ctx, page_key, page_base, prot, generation);

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
    // `write` is loop-invariant; fold it into a permission mask once.
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
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
        if !tlb_prot_allows(prot, need) {
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

/// Software R/W check against a precomputed `need` mask (`TLB_PROT_R` /
/// `TLB_PROT_W`).
///
/// Callers fold the `write` branch into `need` once per helper invocation,
/// hoisting it out of the per-way TLB scans below; each way check then stays
/// a single arithmetic bit test. Mirrors the IR-side fold in
/// [`refill_tlb_probe`](crate::jit::lower::mem::refill_tlb_probe), which
/// computes `need = if write { TLB_PROT_W } else { TLB_PROT_R }` so the
/// emitted probe and the helper share one permission vocabulary.
#[inline]
pub(super) fn tlb_prot_allows(prot: u8, need: u64) -> bool {
    (u64::from(prot) & need) != 0
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
/// On hit, also warms sticky + multi-way TLB **and the inline refill slot**, so
/// the next access to this page never reaches a helper (a miss is what installs
/// the page it just resolved).
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
    // `write` is loop-invariant; fold it into a permission mask once.
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
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
        if !tlb_prot_allows(prot, need) {
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
    ctx.tlb.insert(
        page_key,
        TlbValue {
            host: page_base,
            prot,
        },
        generation,
    );
}

/// Multi-way TLB lookup: scalar way scan (or Neon compare on aarch64).
pub(super) fn tlb_bucket_lookup(
    tlb: &GenTlb<u64, TlbValue, TLB_SETS, TLB_WAYS_PER_SET>,
    page_key: u64,
    write: bool,
    cur_gen: u64,
) -> Option<(*mut u8, u8)> {
    // `write` is loop-invariant here; precompute the permission mask once so
    // the per-way scans below only do arithmetic bit tests.
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    if !JitConfig::get().tlb_neon_enabled() {
        return tlb
            .lookup(page_key, cur_gen, |v| {
                !v.host.is_null() && tlb_prot_allows(v.prot, need)
            })
            .map(|v| (v.host, v.prot));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let s = tlb.set(tlb.set_index(page_key))?;
        // SAFETY: `GenSet` is `align(16)` with `tags` as its first field and
        // `K = u64` here, so `tags` is `WAYS` contiguous 16-byte-aligned u64s.
        let bits = unsafe { tlb_neon_tag_mask(s.tags.as_ptr(), page_key) };
        if bits == 0 {
            return None;
        }
        // First matching way (branchless prefer low index).
        let way = usize::try_from(bits.trailing_zeros()).unwrap_or(0);
        if way >= TLB_WAYS_PER_SET {
            return None;
        }
        let v = s.values.get(way).copied()?;
        if v.host.is_null() {
            return None;
        }
        if s.gens.get(way).copied() != Some(cur_gen) {
            return None;
        }
        if !tlb_prot_allows(v.prot, need) {
            return None;
        }
        Some((v.host, v.prot))
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        tlb.lookup(page_key, cur_gen, |v| {
            !v.host.is_null() && tlb_prot_allows(v.prot, need)
        })
        .map(|v| (v.host, v.prot))
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

/// How a helper-side access was resolved — the `WIE_JIT_MEM_TRACE` histogram axis.
///
/// Produced by [`resolve_page`] on every helper call and read **only** by
/// [`record_resolution`], which is behind one `mem_path_on` branch: the
/// resolution itself never counts, classifies or branches for diagnostics.
#[derive(Clone, Copy)]
enum Resolve {
    /// Host-side sticky victim way.
    Sticky,
    /// 4-way set-associative bucket.
    Multi,
    /// Region-direct pin (stack / heap / VirtualAlloc span).
    Pin,
    /// Page-table walk (first touch of a page under this generation).
    Walk,
    /// The access straddles a page boundary.
    CrossPage,
    /// Resolved, but the current access is not permitted (write to an R page).
    Denied,
    /// Nothing mapped this page.
    Unmapped,
}

/// Everything [`resolve_page`] hands back: the host pointer (if any), how it got
/// there, and whether the host-side sticky scan missed (so the trace can
/// attribute the miss to key / generation / permission).
struct Resolution {
    host: Option<*mut u8>,
    path: Resolve,
    sticky_missed: bool,
}

/// Resolve a single-page guest access to a host pointer (helper side).
///
/// Order: sticky victim ways → 4-way bucket → region pin → page walk. Every
/// resolving branch installs the page into the bucket *and* the inline refill
/// slot (`tlb_set_hot`), so the access that paid this call is the last one.
fn resolve_page(
    ctx: &mut JitCtx,
    addr: u64,
    page_off: usize,
    page_key: u64,
    size: usize,
    write: bool,
) -> Resolution {
    let page_cap = usize::try_from(PAGE_SIZE).unwrap_or(0x1000);
    if page_off.saturating_add(size) > page_cap {
        return Resolution {
            host: None,
            path: Resolve::CrossPage,
            sticky_missed: false,
        };
    }
    let cur_gen = ctx.mem_gen;
    // Sticky victim ways first (the inline refill slot already missed — the
    // caller is a host helper — but a conflict may only have replaced the
    // slot, not the ways).
    //
    // Branchless way select: each way folds key / gen / prot / non-null into one
    // bit, and `pick` becomes `winning_way + 1` (0 = miss). The scan is
    // straight-line arithmetic with a single exit branch instead of up to 4
    // mispredictable `continue`s per way.
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let mut pick = 0_usize; // 0 = miss; else (winning way + 1)
    for w in 0..STICKY_WAYS {
        let key_ok = ctx.sticky_page.get(w).copied() == Some(page_key);
        let host = ctx
            .sticky_ptr
            .get(w)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        let gen_ok = ctx.sticky_gen.get(w).copied() == Some(cur_gen);
        let prot = u8::try_from(ctx.sticky_prot.get(w).copied().unwrap_or(0)).unwrap_or(0);
        let m = usize::from(key_ok & gen_ok & !host.is_null() & tlb_prot_allows(prot, need));
        pick |= m * (w + 1);
    }
    if pick != 0 {
        let w = pick - 1;
        // Re-fetch the winning way by index — no per-way select branch.
        let host = ctx
            .sticky_ptr
            .get(w)
            .copied()
            .unwrap_or(std::ptr::null_mut());
        let prot = u8::try_from(ctx.sticky_prot.get(w).copied().unwrap_or(0)).unwrap_or(0);
        // Keep last-hit mirror coherent with the way that hit.
        ctx.tlb_hot_page = page_key;
        ctx.tlb_hot_ptr = host;
        ctx.tlb_hot_prot = u64::from(prot);
        ctx.tlb_hot_gen = cur_gen;
        // A sticky hit must re-install the inline slot too, or a page whose slot
        // was taken by a conflicting page would resolve through the victim way
        // for ever and never become inline again — measured: a thrashing pair
        // stayed at helpers=1 per access indefinitely, which is exactly the
        // regression this table exists to remove.
        refill_install(ctx, page_key, host, prot, cur_gen);
        return Resolution {
            // SAFETY: sticky ptr is a mapped page base; access stays in-page;
            // SPC bits match.
            host: Some(unsafe { host.add(page_off) }),
            path: Resolve::Sticky,
            sticky_missed: false,
        };
    }
    let hit = tlb_bucket_lookup(&ctx.tlb, page_key, write, cur_gen);
    if let Some((page_base, prot)) = hit {
        tlb_set_hot(ctx, page_key, page_base, prot, cur_gen);
        return Resolution {
            // SAFETY: page mapped; access stays within the page.
            host: Some(unsafe { page_base.add(page_off) }),
            path: Resolve::Multi,
            sticky_missed: true,
        };
    }
    // Region-direct pin (stack/heap arenas) before radix/page walk.
    if let Some(p) = pin_resolve(ctx, addr, size, write) {
        return Resolution {
            host: Some(p),
            path: Resolve::Pin,
            sticky_missed: true,
        };
    }
    // Miss: resolve via GuestMemory (committed + protect meta + host ptr).
    // SAFETY: `mem` set by `run_compiled` to the live guest map.
    let mem = unsafe { &*ctx.mem };
    // `page_tlb_entry_walk` *is* `page_tlb_entry` (mem/rw.rs:275), so the
    // `or_else` fallback here could never fire — it only cost a second walk,
    // with its own `fast_tlb` read, on every miss.
    let Some(entry) = mem.page_tlb_entry_walk(page_key) else {
        return Resolution {
            host: None,
            path: Resolve::Unmapped,
            sticky_missed: true,
        };
    };
    // Install even if this access is denied so a later opposite access can hit;
    // but only return a pointer when the *current* access is allowed.
    let prot = pack_tlb_prot(entry.allow_r, entry.allow_w);
    tlb_install(ctx, page_key, entry.host, prot, entry.generation);
    tlb_set_hot(ctx, page_key, entry.host, prot, entry.generation);
    if !tlb_prot_allows(prot, need) {
        return Resolution {
            host: None,
            path: Resolve::Denied,
            sticky_missed: true,
        };
    }
    Resolution {
        // SAFETY: page mapped; access stays within the page; permission checked.
        host: Some(unsafe { entry.host.add(page_off) }),
        path: Resolve::Walk,
        sticky_missed: true,
    }
}

/// Charge one helper call to the `WIE_JIT_MEM_TRACE` histogram.
///
/// **The only place on the helper path that branches for diagnostics.** Called
/// under `mem_path_on`, and when that flag is clear neither this nor
/// [`classify_addr_vs_pins`] (a loop over every pin) nor [`classify_sticky_miss`]
/// (a second way scan) runs at all — the counters exist for one trace knob and
/// nothing else reads them.
fn record_resolution(
    ctx: &mut JitCtx,
    addr: u64,
    size: usize,
    page_key: u64,
    write: bool,
    r: &Resolution,
) {
    classify_addr_vs_pins(ctx, addr, size);
    let n = |v: u64| v.saturating_add(1);
    match r.path {
        Resolve::Sticky => ctx.mem_path.sticky_hit = n(ctx.mem_path.sticky_hit),
        Resolve::Multi => ctx.mem_path.multi_hit = n(ctx.mem_path.multi_hit),
        Resolve::Pin => ctx.mem_path.pin_hit = n(ctx.mem_path.pin_hit),
        Resolve::Walk => ctx.mem_path.walk_hit = n(ctx.mem_path.walk_hit),
        Resolve::CrossPage => ctx.mem_path.cross_page = n(ctx.mem_path.cross_page),
        Resolve::Denied | Resolve::Unmapped => ctx.mem_path.slow = n(ctx.mem_path.slow),
    }
    if r.sticky_missed {
        classify_sticky_miss(ctx, page_key, write);
    }
}

/// Resolve host page pointer for a helper load/store (single-page accesses only).
///
/// Enforces software R/W bits and memory generation on every hit. Misses that
/// lack the required permission return `None` so the slow path can set a fault
/// via [`GuestMemory::read`] / [`GuestMemory::write`] (SPC oracle).
///
/// Every resolution installs into the inline refill table, so the next access
/// to that page never reaches this function.
pub(super) unsafe fn tlb_page_ptr(
    ctx: &mut JitCtx,
    addr: u64,
    size: usize,
    write: bool,
) -> Option<*mut u8> {
    let page_off = usize::try_from(addr & (PAGE_SIZE - 1)).unwrap_or(0);
    let page_key = addr >> 12; // PAGE_SIZE = 0x1000
    let r = resolve_page(ctx, addr, page_off, page_key, size, write);
    if ctx.mem_path_on != 0 {
        record_resolution(ctx, addr, size, page_key, write, &r);
    }
    r.host
}
