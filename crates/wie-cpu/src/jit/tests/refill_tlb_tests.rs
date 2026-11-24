//! The inline mem fast path: a **direct-mapped** refill table in `JitCtx`.
//!
//! What is under test is not "does a guest load work" (every other suite covers
//! that) but the *table*: that a page the helper resolved becomes an inline hit,
//! that the emitted validity predicate can be trusted, and that everything the
//! probe is allowed to reject still reaches the helper and faults exactly where
//! it used to. `docs/architecture/tlb-probe-redesign.md` motivates the shape;
//! this file is the contract.
//!
//! The path each access took is read from `ctx.load_calls` / `ctx.store_calls` —
//! the per-run host-helper counters, reset at every dispatcher entry. An inline
//! hit is exactly "this access did not call the helper", which is the property
//! that matters and is measurable without a wall clock.
//!
//! ## Fixtures
//!
//! One guest page of code per test (`alloc_code`, so each encoding keeps its own
//! VA — see the harness note on the process-wide decode cache) holding a single
//! access through `r15`, one address register the test re-points between runs.
//! Re-entering the *same* compiled block with a different `r15` is how these
//! tests walk a working set: one block, N pages.

use super::*;
use crate::CpuEngine;
use crate::exec::{AccessType, InvalidMem, StepResult};
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_DECOMMIT, MEM_RESERVE};
use crate::regs::Rflags;
use lower::{
    GOLDEN_RATIO_64, REFILL_OFFSET_MASK, REFILL_SHIFT, REFILL_SLOTS, REFILL_STRIDE,
    REFILL_STRIDE_SHIFT, RefillSlot, TLB_PROT_R, TLB_PROT_W, refill_slot,
};

/// Where the block jumps once its access is done. The dispatcher stops before
/// executing it, so the `ud2` there never runs — it only marks "block finished".
const STOP_OFF: u64 = 0x100;

/// `mov rax,[r15]` — one 8-byte load.
const LOAD64_R15: [u8; 3] = [0x49, 0x8b, 0x07];
/// `mov [r15],rax` — one 8-byte store.
const STORE64_R15: [u8; 3] = [0x49, 0x89, 0x07];

/// Lay out a one-access block at a fresh code page and return its VA.
fn code_page(cpu: &mut JitCpu, insn: [u8; 3]) -> u64 {
    let base = alloc_code_page(cpu);
    let stop = usize::try_from(STOP_OFF).unwrap_or(0x100);
    let mut page = vec![0x90_u8; stop + 2];
    page[0..3].copy_from_slice(&insn);
    // `jmp rel32` to STOP: the jump starts at +3 and ends at +8.
    let rel = i32::try_from(STOP_OFF.saturating_sub(8)).unwrap_or(0);
    page[3] = 0xe9;
    page[4..8].copy_from_slice(&rel.to_le_bytes());
    page[stop] = 0x0f; // ud2
    page[stop + 1] = 0x0b;
    cpu.mem_write(base, &page).expect("write code page");
    base
}

/// Run the block at `code` once with `r15 = addr`; return `(rax, rip)`.
fn run_at(cpu: &mut JitCpu, code: u64, addr: u64) -> (u64, u64) {
    let regs = &mut cpu.thread.regs;
    regs.set_gpr(15, addr);
    regs.set_rflags_checked(Rflags::from(0x202_u64));
    cpu.write_rip(code).expect("set rip");
    let (result, _retired) = cpu.step_one().expect("step");
    assert!(
        matches!(result, StepResult::Continue),
        "block at {code:#x} with r15={addr:#x} did not retire: {result:?}"
    );
    (cpu.thread.regs.gpr(0), cpu.thread.regs.rip)
}

/// Same, for a run the test expects to fault.
fn run_at_expect_fault(cpu: &mut JitCpu, code: u64, addr: u64) -> InvalidMem {
    let regs = &mut cpu.thread.regs;
    regs.set_gpr(15, addr);
    regs.set_rflags_checked(Rflags::from(0x202_u64));
    cpu.write_rip(code).expect("set rip");
    match cpu.step_one().expect("step").0 {
        StepResult::InvalidMemory(inv) => inv,
        other => panic!("block at {code:#x} with r15={addr:#x}: expected a fault, got {other:?}"),
    }
}

/// Map `pages` RWX pages at `base` and stamp page `i`'s first 8 bytes with `i`,
/// so a load tells the test which page it actually read.
fn map_marked(cpu: &mut JitCpu, base: u64, pages: usize) {
    let size = pages.saturating_mul(0x1000);
    cpu.virtual_alloc(
        base,
        size,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("map data region");
    for i in 0..pages {
        let va = base.saturating_add(u64::try_from(i).unwrap_or(0).saturating_mul(0x1000));
        cpu.mem_write(va, &u64::try_from(i).unwrap_or(0).to_ne_bytes())
            .expect("stamp page marker");
    }
}

fn read_u64(cpu: &mut JitCpu, addr: u64) -> u64 {
    let mut buf = [0_u8; 8];
    cpu.mem_read(addr, &mut buf).expect("read u64");
    u64::from_ne_bytes(buf)
}

/// The slot `page_key` maps to, as installed code sees it.
fn slot_of(cpu: &JitCpu, page_key: u64) -> RefillSlot {
    cpu.thread.ctx.refill[refill_slot(page_key)]
}

// ── The probe's own arithmetic ──────────────────────────────────────────

/// The emitted index and [`refill_slot`] must be the same computation: the probe
/// computes `band_imm(ushr_imm(key * GOLDEN, 51), REFILL_OFFSET_MASK)` from
/// immediates, the install side calls `refill_slot`. If they ever drift, every
/// install lands in a slot no probe reads — silently, and at full speed.
#[test]
fn refill_slot_matches_emitted_index() {
    let mask = u64::try_from(REFILL_OFFSET_MASK).unwrap_or(0);
    for page_key in [
        0_u64,
        1,
        2,
        0xff,
        0x100,
        0x1234,
        0x7fff_ffff,
        0x1_0000_0000,
        u64::from(u32::MAX),
        0x0000_7fff_ffff_f000_u64 >> 12,
    ] {
        let h = page_key.wrapping_mul(GOLDEN_RATIO_64);
        let emitted = (h >> (REFILL_SHIFT - REFILL_STRIDE_SHIFT)) & mask;
        assert_eq!(
            usize::try_from(emitted).unwrap_or(usize::MAX),
            refill_slot(page_key).saturating_mul(REFILL_STRIDE),
            "emitted index and refill_slot disagree for page_key {page_key:#x}"
        );
        assert!(refill_slot(page_key) < REFILL_SLOTS);
    }
}

/// The scatter must actually scatter: every slot reachable, and no two
/// *adjacent* page keys in the same slot. The second property is what keeps a
/// sequentially walked buffer (`[r15]`, `[r15+0x1000]`, …) resident instead of
/// thrashing one slot, and it is arithmetic rather than luck: adding the golden
/// ratio to the product moves bits 56..64 by its top byte, so `slot(k+1)`'s
/// offset differs from `slot(k)`'s by `0x9E` or `0x9F` — never 0.
#[test]
fn refill_scatter_is_well_distributed() {
    let mut seen = [false; REFILL_SLOTS];
    let mut prev = refill_slot(0);
    if let Some(s) = seen.get_mut(prev) {
        *s = true;
    }
    for page_key in 1..(1_u64 << 16) {
        let slot = refill_slot(page_key);
        assert!(
            slot != prev,
            "adjacent page keys {:#x}/{page_key:#x} share slot {slot}",
            page_key - 1
        );
        if let Some(s) = seen.get_mut(slot) {
            *s = true;
        }
        prev = slot;
    }
    assert!(
        seen.iter().all(|&s| s),
        "the scatter left slots unreachable over 64Ki keys"
    );
}

/// The folded tag answers both questions, and it is the *only* thing that
/// rejects a slot: a generation bump, a different page, or an insufficient
/// permission must all miss.
#[test]
fn slot_tag_folds_key_and_generation() {
    let page_key = 0x1_2345_u64;
    let installed = RefillSlot::new(page_key ^ 7, 0x1000_usize as *mut u8, TLB_PROT_R);
    assert!(installed.hits(page_key, 7, TLB_PROT_R));
    assert!(
        !installed.hits(page_key, 8, TLB_PROT_R),
        "a bumped generation must reject the slot: that is what invalidates the \
         inline path on VirtualProtect/VirtualFree"
    );
    assert!(!installed.hits(page_key ^ 1, 7, TLB_PROT_R), "wrong page");
    assert!(
        !installed.hits(page_key, 7, TLB_PROT_W),
        "an R-only slot must not serve a store"
    );
    assert!(
        !RefillSlot::EMPTY.hits(0, 0, TLB_PROT_R),
        "an empty slot must never match"
    );
    assert!(
        !RefillSlot::new(0, std::ptr::null_mut(), TLB_PROT_R).hits(0, 0, TLB_PROT_R),
        "a slot with no host base must never match"
    );
}

// ── Miss → install → hit ────────────────────────────────────────────────

/// The access that pays the helper installs the page it resolved, so the next
/// access to that page is inline. That is the whole point of the change: the
/// helper used to be paid by ~99.98 % of accesses.
#[test]
fn cold_access_installs_the_page_and_the_next_one_hits() {
    const DATA: u64 = 0x6000_0000;
    let mut cpu = JitCpu::open_x86_64();
    let code = code_page(&mut cpu, LOAD64_R15);
    map_marked(&mut cpu, DATA, 1);

    // Cold: the slot is empty, so the load takes the helper, which resolves the
    // page through the walk and installs it.
    let (rax, rip) = run_at(&mut cpu, code, DATA);
    assert_eq!(rip, code + STOP_OFF, "block must end at STOP");
    assert_eq!(rax, 0, "page 0 marker");
    assert_eq!(
        cpu.thread.ctx.load_calls, 1,
        "a cold access must call the load helper exactly once"
    );

    // The `mem_path` diagnostics are behind one branch and nothing else reads
    // them: on a default run (no `WIE_JIT_MEM_TRACE`) a helper call must leave
    // every counter — including the pin classification loop — at zero.
    if !config::JitConfig::get().mem_path_trace_enabled() {
        let m = &cpu.thread.ctx.mem_path;
        assert_eq!(
            m.walk_hit
                .saturating_add(m.multi_hit)
                .saturating_add(m.pin_hit)
                .saturating_add(m.sticky_hit)
                .saturating_add(m.slow)
                .saturating_add(m.cross_page),
            0,
            "the mem-path counters must not run unless WIE_JIT_MEM_TRACE is set"
        );
        assert_eq!(
            m.addr_in_stack_pin
                .saturating_add(m.addr_in_heap_pin)
                .saturating_add(m.addr_outside_pins),
            0,
            "classify_addr_vs_pins is diagnostic-only and must not run by default"
        );
    }

    let page_key = DATA >> 12;
    let cur_gen = cpu.thread.ctx.mem_gen;
    let slot = slot_of(&cpu, page_key);
    assert_eq!(
        slot.tag,
        page_key ^ cur_gen,
        "the installed tag must fold the page key and the frame's generation"
    );
    assert!(!slot.base.is_null(), "the slot must carry a host page base");
    assert_eq!(slot.prot, TLB_PROT_R, "an RWX page soft-translates reads");
    assert!(
        slot.hits(page_key, cur_gen, TLB_PROT_R),
        "the slot the helper just installed must be honoured by the probe"
    );

    // Warm: same block, same page. Zero helper calls means the emitted probe
    // served it — the helper is the only other way to translate.
    let (rax, _) = run_at(&mut cpu, code, DATA);
    assert_eq!(rax, 0);
    assert_eq!(
        cpu.thread.ctx.load_calls, 0,
        "the second access to an installed page must not reach the helper"
    );
}

// ── Invalidation ────────────────────────────────────────────────────────

/// A memory-generation bump (any `VirtualProtect` / `VirtualFree` / remap) must
/// make the same access miss again, and the helper must re-install it under the
/// new generation.
#[test]
fn generation_bump_makes_the_same_access_miss() {
    const DATA: u64 = 0x6010_0000;
    let mut cpu = JitCpu::open_x86_64();
    let code = code_page(&mut cpu, LOAD64_R15);
    map_marked(&mut cpu, DATA, 1);

    let _ = run_at(&mut cpu, code, DATA); // install
    let _ = run_at(&mut cpu, code, DATA); // inline hit
    assert_eq!(
        cpu.thread.ctx.load_calls, 0,
        "warm-up must have hit the slot"
    );
    let page_key = DATA >> 12;
    let gen_before = cpu.thread.ctx.mem_gen;

    // Re-protecting the page bumps the guest memory generation.
    cpu.virtual_protect(DATA, 0x1000, protect::PAGE_READONLY)
        .expect("protect ro");
    assert!(
        cpu.thread.ctx.refill[refill_slot(page_key)].base.is_null(),
        "a generation change must drop the refill table: no slot may outlive the \
         mapping it was resolved under"
    );

    let (rax, _) = run_at(&mut cpu, code, DATA);
    assert_eq!(rax, 0);
    assert_eq!(
        cpu.thread.ctx.load_calls, 1,
        "after a generation bump the access must take the helper"
    );
    let gen_after = cpu.thread.ctx.mem_gen;
    assert!(
        gen_after > gen_before,
        "the frame must observe the new generation ({gen_after} vs {gen_before})"
    );
    let slot = slot_of(&cpu, page_key);
    assert_eq!(
        slot.tag,
        page_key ^ gen_after,
        "the re-installed tag must carry the new generation"
    );
    assert!(slot.hits(page_key, gen_after, TLB_PROT_R));
    assert!(
        !slot.hits(page_key, gen_before, TLB_PROT_R),
        "the old generation must not be honoured by the new slot"
    );

    // …and it is warm again under the new generation.
    let _ = run_at(&mut cpu, code, DATA);
    assert_eq!(cpu.thread.ctx.load_calls, 0);
}

// ── Collision / thrash ──────────────────────────────────────────────────

/// A direct-mapped table cannot avoid conflicts; it must only make them
/// *correct*. Two pages that scatter to the same slot must each resolve
/// correctly, whichever of them the slot happens to hold.
#[test]
fn colliding_pages_on_one_slot_still_resolve_correctly() {
    // 1024 pages into 256 slots: by pigeonhole a collision exists by
    // construction rather than by luck, and the deterministic hash means the
    // same pair every run.
    const DATA: u64 = 0x6100_0000;
    const PAGES: usize = 1024;
    let mut cpu = JitCpu::open_x86_64();
    let code = code_page(&mut cpu, LOAD64_R15);
    map_marked(&mut cpu, DATA, PAGES);

    let base_key = DATA >> 12;
    let mut first_in_slot: [Option<usize>; REFILL_SLOTS] = [None; REFILL_SLOTS];
    let mut pair = None;
    for i in 0..PAGES {
        let page_key = base_key.saturating_add(u64::try_from(i).unwrap_or(0));
        let slot = refill_slot(page_key);
        match first_in_slot.get(slot).copied().flatten() {
            Some(other) => {
                pair = Some((other, i));
                break;
            }
            None => {
                if let Some(s) = first_in_slot.get_mut(slot) {
                    *s = Some(i);
                }
            }
        }
    }
    let (a, b) = pair.expect("1024 pages must collide in 256 slots");
    let addr_of =
        |i: usize| DATA.saturating_add(u64::try_from(i).unwrap_or(0).saturating_mul(0x1000));
    let (addr_a, addr_b) = (addr_of(a), addr_of(b));
    let marker = |i: usize| u64::try_from(i).unwrap_or(0);
    let slot = refill_slot(addr_a >> 12);
    assert_eq!(
        slot,
        refill_slot(addr_b >> 12),
        "the chosen pair must share a slot"
    );

    // Cold A, warm A, B (evicts A), A again (helper: evicted), A (warm again).
    // Every run must read its own page's marker.
    for (addr, want_marker, helpers) in [
        (addr_a, marker(a), 1_u64),
        (addr_a, marker(a), 0),
        (addr_b, marker(b), 1),
        (addr_a, marker(a), 1),
        (addr_a, marker(a), 0),
    ] {
        let (rax, _) = run_at(&mut cpu, code, addr);
        assert_eq!(
            rax, want_marker,
            "r15={addr:#x}: must read the marker of the page it addressed"
        );
        assert_eq!(
            cpu.thread.ctx.load_calls, helpers,
            "r15={addr:#x}: helper calls (slot {slot} shared by {addr_a:#x} and {addr_b:#x})"
        );
    }
}

// ── Fault semantics ──────────────────────────────────────────────────────

/// A cross-page access must take the helper even when the slot for its page is
/// installed and valid, and it must fault exactly where it used to.
#[test]
fn cross_page_access_takes_the_helper_and_faults_where_it_did() {
    const DATA: u64 = 0x6200_0000;
    let mut cpu = JitCpu::open_x86_64();
    let code = code_page(&mut cpu, LOAD64_R15);
    // Reserve two pages and decommit the second: `VirtualAlloc` rounds a commit
    // up to the 64 KiB allocation granularity, so a plain one-page allocation
    // would leave the neighbour committed and the straddle legal.
    map_marked(&mut cpu, DATA, 2);
    cpu.virtual_free(DATA.saturating_add(0x1000), 0x1000, MEM_DECOMMIT)
        .expect("decommit page 1");

    // Install the page with in-page accesses.
    let (rax, _) = run_at(&mut cpu, code, DATA);
    assert_eq!(rax, 0);
    let _ = run_at(&mut cpu, code, DATA);
    assert_eq!(cpu.thread.ctx.load_calls, 0, "the slot must be warm");
    let page_key = DATA >> 12;
    assert!(slot_of(&cpu, page_key).hits(page_key, cpu.thread.ctx.mem_gen, TLB_PROT_R));

    // 8 bytes at page_end-4 straddles into a page that is not mapped. The slot
    // for the *first* page is warm and valid, so the page-crossing predicate is
    // the only thing that can send this access to the helper.
    let straddle = DATA.saturating_add(0xffc);
    let inv = run_at_expect_fault(&mut cpu, code, straddle);
    assert_eq!(inv.address, straddle, "fault address");
    assert_eq!(inv.size, 8, "fault size");
    assert_eq!(inv.access_type, AccessType::Read, "fault access");
    assert_eq!(
        cpu.thread.ctx.load_calls, 1,
        "a cross-page access must take the helper, not the slot"
    );
}

/// The same straddling access over two *mapped* pages still resolves through the
/// helper (the inline probe refuses it) and returns the right bytes.
#[test]
fn cross_page_access_over_mapped_pages_resolves_through_the_helper() {
    const DATA: u64 = 0x6210_0000;
    let mut cpu = JitCpu::open_x86_64();
    let code = code_page(&mut cpu, LOAD64_R15);
    map_marked(&mut cpu, DATA, 2);
    // Straddle value: page 0's last 4 bytes low, page 1's first 4 bytes high.
    let straddle = DATA.saturating_add(0xffc);
    cpu.mem_write(straddle, &0x1122_3344_u32.to_le_bytes())
        .expect("tail of page 0");
    cpu.mem_write(DATA.saturating_add(0x1000), &0x5566_7788_u32.to_le_bytes())
        .expect("head of page 1");

    // Warm both slots with in-page accesses first, so a cross-page access is the
    // only reason left for a helper call.
    let _ = run_at(&mut cpu, code, DATA);
    let _ = run_at(&mut cpu, code, DATA.saturating_add(0x1000));
    let _ = run_at(&mut cpu, code, DATA);
    assert_eq!(cpu.thread.ctx.load_calls, 0, "both pages must be warm");

    let (rax, _) = run_at(&mut cpu, code, straddle);
    assert_eq!(
        rax, 0x5566_7788_1122_3344,
        "a straddling load must concatenate across the page boundary"
    );
    assert_eq!(
        cpu.thread.ctx.load_calls, 1,
        "a straddling load must take the helper"
    );
}

/// A store to a page whose slot says "readable, not writable" must take the
/// helper and fault as a guest write — the permission word in the slot is the
/// only thing standing between a denied store and a silent host write.
#[test]
fn denied_store_takes_the_helper_and_faults() {
    const DATA: u64 = 0x6300_0000;
    let mut cpu = JitCpu::open_x86_64();
    let load_code = code_page(&mut cpu, LOAD64_R15);
    let store_code = code_page(&mut cpu, STORE64_R15);
    map_marked(&mut cpu, DATA, 1);

    // Read-only page: the walk installs it with the R bit only, which is what a
    // later store has to be rejected by.
    cpu.virtual_protect(DATA, 0x1000, protect::PAGE_READONLY)
        .expect("protect ro");
    let (rax, _) = run_at(&mut cpu, load_code, DATA);
    assert_eq!(rax, 0);
    let page_key = DATA >> 12;
    assert_eq!(
        slot_of(&cpu, page_key).prot,
        TLB_PROT_R,
        "an R-only page installs R only"
    );
    let _ = run_at(&mut cpu, load_code, DATA);
    assert_eq!(
        cpu.thread.ctx.load_calls, 0,
        "reads of the R-only page are inline"
    );

    // Store: the slot's R bit does not satisfy `need = W`, so the probe misses,
    // the helper refuses, and the guest sees the fault it always saw.
    let before = read_u64(&mut cpu, DATA);
    let inv = run_at_expect_fault(&mut cpu, store_code, DATA);
    assert_eq!(inv.access_type, AccessType::Write, "must fault as a write");
    assert_eq!(inv.address, DATA, "fault address");
    assert_eq!(
        cpu.thread.ctx.store_calls, 1,
        "a denied store must take the helper"
    );
    assert_eq!(
        read_u64(&mut cpu, DATA),
        before,
        "a faulting store must not write through"
    );
    // The denied page stays installed (an opposite access may be legal), with
    // the permission word that produced the denial.
    assert_eq!(slot_of(&cpu, page_key).prot, TLB_PROT_R);
}
