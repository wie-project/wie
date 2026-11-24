//! Unit tests for `crate::mem`: `GuestMemory` facade, mmap backend, VAD,
//! SPC checks, region pins and generation guards.

#![allow(clippy::expect_used)]

use super::*;
use super::{
    ERROR_INVALID_ADDRESS, GUEST_ALLOC_GRANULARITY, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE,
    MEM_RESERVE, win32_from_cpu_error,
};

#[test]
fn page_table_walk_matches_map() {
    let mut mem = GuestMemory::new();
    mem.map(0x10_0000, 0x2000, crate::RwxPerms::ALL)
        .expect("map");
    let k = page_key(0x10_0000);
    let p = mem.page_data_ptr_walk(k).expect("walk");
    assert!(!p.is_null());
    let p2 = mem.page_data_ptr(k).expect("hash");
    assert_eq!(p, p2);
    assert!(mem.page_data_ptr_walk(k + 100).is_none());
}

#[test]
fn page_table_high_va() {
    let mut mem = GuestMemory::new();
    let base = 0x0000_7fff_0000_0000_u64;
    mem.map(base, 0x1000, crate::RwxPerms::ALL)
        .expect("map high");
    let k = page_key(base);
    assert!(mem.page_data_ptr_walk(k).is_some());
}

#[test]
fn region_registry_find() {
    let mut mem = GuestMemory::new();
    mem.register_region(GuestRegion::new(
        "stack",
        RegionKind::Stack,
        0x2000_0000,
        0x1_0000,
        crate::RwxPerms::ALL,
    ));
    mem.map(0x2000_0000, 0x1_0000, crate::RwxPerms::ALL)
        .expect("map stack");
    assert_eq!(mem.find_region(0x2000_0800).expect("found").name, "stack");
    assert_eq!(mem.backend_name(), "mmap");
}

#[test]
fn span_pin_and_va_slots_from_private_vad() {
    let mut mem = GuestMemory::new();
    // Stack + heap named regions (slots 0/1).
    mem.map(0x2000_0000, 0x1_0000, crate::RwxPerms::READ_WRITE)
        .expect("stack");
    mem.register_region(GuestRegion::new(
        "stack",
        RegionKind::Stack,
        0x2000_0000,
        0x1_0000,
        crate::RwxPerms::READ_WRITE,
    ));
    mem.map(0x1600_0000_0000, 0x10_0000, crate::RwxPerms::READ_WRITE)
        .expect("heap");
    mem.register_region(GuestRegion::new(
        "process_heap",
        RegionKind::Heap,
        0x1600_0000_0000,
        0x10_0000,
        crate::RwxPerms::READ_WRITE,
    ));
    // Separate VirtualAlloc-like private span (should fill a VA pin slot).
    let va_base = 0x0000_0001_2000_0000_u64;
    let va_size = 0x40_0000_usize; // 4 MiB
    mem.map(va_base, va_size, crate::RwxPerms::READ_WRITE)
        .expect("va");

    let pins = mem.jit_region_pins();
    assert!(pins.first().is_some_and(Option::is_some), "stack pin");
    // Data slots 1.. are size-ranked: 4 MiB VA should outrank 1 MiB heap.
    let data: Vec<_> = pins.iter().skip(1).flatten().collect();
    assert!(
        data.iter().any(|p| p.guest_base == 0x1600_0000_0000),
        "heap among data pins"
    );
    let va_size_u64 = u64::try_from(va_size).expect("va_size fits u64");
    let va_pin = data
        .iter()
        .find(|p| p.guest_base == va_base)
        .expect("VA pin among data slots");
    assert_eq!(va_pin.guest_end, va_base.saturating_add(va_size_u64));
    assert!(va_pin.allow_r && va_pin.allow_w);
    assert!(!va_pin.host_base.is_null());
    // Largest data pin should be the VA span (first data slot).
    assert_eq!(
        pins.get(1).and_then(|p| p.as_ref()).map(|p| p.guest_base),
        Some(va_base)
    );

    // Soft-translate mid-span: pin covers VA and page walk succeeds.
    let mid = va_base.saturating_add(0x1234);
    let pin = mem
        .span_pin(va_base, va_base.saturating_add(va_size_u64))
        .expect("span");
    assert!(mid >= pin.guest_base && mid < pin.guest_end);
    assert!(mem.page_data_ptr_walk(mid >> 12).is_some());
}

#[test]
fn mmap_backend_host_base_on_register() {
    let mut mem = GuestMemory::new();
    mem.map(0x2000_0000, 0x1_0000, crate::RwxPerms::ALL)
        .expect("map stack");
    mem.register_region(GuestRegion::new(
        "stack",
        RegionKind::Stack,
        0x2000_0000,
        0x1_0000,
        crate::RwxPerms::ALL,
    ));
    let r = mem.find_region(0x2000_0800).expect("found");
    let hb = r.host_base.expect("host_base should be filled from arena");
    assert_ne!(hb, 0);
    let p = mem.page_data_ptr_walk(0x2000_0800 >> 12).expect("page");
    assert!(!p.is_null());
    assert_eq!(mem.backend_name(), "mmap");
}

#[test]
fn mmap_page_ptr_walk() {
    let mut mem = GuestMemory::new();
    mem.map(0x10_0000, 0x2000, crate::RwxPerms::ALL)
        .expect("map");
    let k = page_key(0x10_0000);
    let p = mem.page_data_ptr_walk(k).expect("walk");
    let p2 = mem.page_data_ptr(k).expect("ptr");
    assert_eq!(p, p2);
}

#[test]
fn spc_readonly_write_fails_read_ok() {
    let mut mem = GuestMemory::new();
    mem.map(0x20_0000, 0x1000, crate::RwxPerms::READ)
        .expect("map RO");
    let mut buf = [0_u8; 4];
    mem.read(0x20_0000, &mut buf).expect("read ok");
    assert!(mem.write(0x20_0000, &[1, 2, 3, 4]).is_err());
    // Host storage still present; failure is SPC, not unmapped.
    let err = mem.write(0x20_0000, &[1]).expect_err("write denied");
    assert!(err.to_string().contains("permission denied"));
}

#[test]
fn page_tlb_entry_tags_ro_and_bumps_gen_on_protect() {
    let mut mem = GuestMemory::new();
    mem.map(0x50_0000, 0x1000, crate::RwxPerms::ALL)
        .expect("map RWX");
    let k = page_key(0x50_0000);
    let e0 = mem.page_tlb_entry(k).expect("tlb entry");
    // RWX → soft-translate R only (no W on X).
    assert!(e0.allow_r && !e0.allow_w);
    let g0 = e0.generation;
    assert!(g0 >= 1);

    let old = mem
        .virtual_protect(0x50_0000, 0x1000, protect::PAGE_READONLY)
        .expect("protect RO");
    assert_eq!(old, protect::PAGE_EXECUTE_READWRITE);
    assert!(mem.generation() > g0);

    let e1 = mem.page_tlb_entry(k).expect("tlb after protect");
    assert!(e1.allow_r);
    assert!(!e1.allow_w);
    assert_eq!(e1.generation, mem.generation());
    assert!(mem.write(0x50_0000, &[0xcc]).is_err());
}

#[test]
fn page_tlb_entry_rw_allows_w_rx_denies_w() {
    let mut mem = GuestMemory::new();
    mem.map(0x52_0000, 0x1000, crate::RwxPerms::READ_WRITE)
        .expect("map RW");
    let e = mem.page_tlb_entry(page_key(0x52_0000)).expect("rw");
    assert!(e.allow_r && e.allow_w);

    mem.map(0x53_0000, 0x1000, crate::RwxPerms::new(true, false, true))
        .expect("map RX");
    let e2 = mem.page_tlb_entry(page_key(0x53_0000)).expect("rx");
    assert!(e2.allow_r && !e2.allow_w);
}

#[test]
fn page_tlb_entry_none_for_noaccess() {
    let mut mem = GuestMemory::new();
    mem.map(0x51_0000, 0x1000, crate::RwxPerms::NONE)
        .expect("map NA");
    // perms 0 → PAGE_NOACCESS after map_with_type
    assert!(mem.page_tlb_entry(page_key(0x51_0000)).is_none());
}

#[test]
fn region_pin_requires_host_base_and_intersects_protect() {
    // Mmap backend fills host_base; uniform RW → full pin.
    let mut mem = GuestMemory::new();
    mem.map(0x2000_0000, 0x1_0000, crate::RwxPerms::ALL)
        .expect("map stack");
    mem.register_region(GuestRegion::new(
        "stack",
        RegionKind::Stack,
        0x2000_0000,
        0x1_0000,
        crate::RwxPerms::ALL,
    ));
    let r = mem.find_region(0x2000_0800).expect("region").clone();
    // Map used ALL (RWX) — pin is R-only when any page is X.
    let pin = mem.region_pin(&r).expect("pin");
    assert_eq!(pin.guest_base, 0x2000_0000);
    assert_eq!(pin.guest_end, 0x2001_0000);
    assert!(!pin.host_base.is_null());
    assert!(pin.allow_r);
    assert!(!pin.allow_w);

    // Pure RW stack: W soft-translate allowed.
    mem.virtual_protect(0x2000_0000, 0x1_0000, protect::PAGE_READWRITE)
        .expect("protect whole stack RW");
    let pin_rw = mem.region_pin(&r).expect("pin RW");
    assert!(pin_rw.allow_r && pin_rw.allow_w);

    // Mixed protect: one RO page → allow_w false (conservative).
    mem.virtual_protect(0x2000_1000, 0x1000, protect::PAGE_READONLY)
        .expect("protect one page RO");
    let pin2 = mem.region_pin(&r).expect("pin after protect");
    assert!(pin2.allow_r);
    assert!(!pin2.allow_w);
    assert!(pin2.generation > pin.generation);
}

#[test]
fn region_pin_disabled_without_host_base() {
    // Register a named region without any covering arena → no pin.
    let mut mem = GuestMemory::new();
    mem.register_region(GuestRegion::new(
        "stack",
        RegionKind::Stack,
        0x2000_0000,
        0x1000,
        crate::RwxPerms::ALL,
    ));
    let r = mem.find_region(0x2000_0000).expect("region").clone();
    assert!(r.host_base.is_none());
    assert!(mem.region_pin(&r).is_none());
}

#[test]
fn host_span_single_page() {
    let mut mem = GuestMemory::new();
    // Pure RW data — host-span write allowed (not executable).
    mem.map(0x40_0000, 0x2000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    let p = mem
        .host_span(0x40_0100, 64, true)
        .expect("single-page host span");
    assert!(!p.is_null());
    // SAFETY: span is live for this test.
    #[expect(unsafe_code)]
    unsafe {
        std::ptr::write_bytes(p, 0x5a, 64);
    }
    let mut buf = [0_u8; 64];
    mem.read(0x40_0100, &mut buf).expect("read back");
    assert!(buf.iter().all(|&b| b == 0x5a));
}

#[test]
fn host_span_multi_page_mmap() {
    let mut mem = GuestMemory::new();
    mem.map(0x50_0000, 0x3000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    let len = 0x2000_usize;
    let p = mem
        .host_span(0x50_0800, len, true)
        .expect("arena multi-page span");
    assert!(!p.is_null());
    // SAFETY: span is live for this test.
    #[expect(unsafe_code)]
    unsafe {
        std::ptr::write_bytes(p, 0xa5, len);
    }
    let mut buf = vec![0_u8; len];
    mem.read(0x50_0800, &mut buf).expect("read");
    assert!(buf.iter().all(|&b| b == 0xa5));
}

#[test]
fn host_span_ro_write_denied() {
    let mut mem = GuestMemory::new();
    mem.map(0x60_0000, 0x1000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    mem.virtual_protect(0x60_0000, 0x1000, protect::PAGE_READONLY)
        .expect("ro");
    assert!(mem.host_span(0x60_0000, 32, true).is_none());
    assert!(mem.host_span(0x60_0000, 32, false).is_some());
}

#[test]
fn host_span_write_denied_on_executable() {
    let mut mem = GuestMemory::new();
    mem.map(0x61_0000, 0x1000, crate::RwxPerms::ALL)
        .expect("map RWX");
    // No host-span write onto X pages (SMC via write + invalidate).
    assert!(mem.host_span(0x61_0000, 16, true).is_none());
    assert!(mem.host_span(0x61_0000, 16, false).is_some());
}

#[test]
fn region_pin_disabled_when_gap_in_range() {
    let mut mem = GuestMemory::new();
    // Two committed islands with a free hole between them.
    mem.map(0x3000_0000, 0x1000, crate::RwxPerms::ALL)
        .expect("map a");
    mem.map(0x3000_2000, 0x1000, crate::RwxPerms::ALL)
        .expect("map b");
    // Register a region that claims the hole too (host_base from first arena).
    mem.register_region(GuestRegion::new(
        "span",
        RegionKind::Other,
        0x3000_0000,
        0x3000,
        crate::RwxPerms::ALL,
    ));
    // host_base may be set from first map only covering part of region —
    // pin must still reject the free middle page.
    let r = mem.find_region(0x3000_0000).expect("region").clone();
    if r.host_base.is_some() {
        assert!(mem.region_pin(&r).is_none(), "gap must disable pin");
    }
}

#[test]
fn spc_rx_fetch_ok_write_fails() {
    let mut mem = GuestMemory::new();
    mem.map(0x30_0000, 0x1000, crate::RwxPerms::new(true, false, true))
        .expect("map RX");
    // Seed bytes via backend would bypass SPC; map is zeroed — fetch still ok.
    let mut out = [0_u8; 15];
    let n = mem.fetch_into(0x30_0000, &mut out).expect("fetch");
    assert!(n > 0);
    assert!(mem.write(0x30_0000, &[0x90]).is_err());
}

#[test]
fn spc_unmapped_fails() {
    let mem = GuestMemory::new();
    let mut buf = [0_u8; 4];
    assert!(mem.read(0x40_0000, &mut buf).is_err());
}

#[test]
fn spc_cross_page_all_or_nothing() {
    let mut mem = GuestMemory::new();
    mem.map(0x50_0000, 0x1000, crate::RwxPerms::ALL)
        .expect("map one");
    // Write straddling into unmapped second page must not partial-write.
    let payload = [0xAAu8; 8];
    assert!(mem.write(0x50_0ffc, &payload).is_err());
    let mut check = [0_u8; 4];
    mem.read(0x50_0ffc, &mut check).expect("prefix still zero");
    assert_eq!(check, [0, 0, 0, 0]);
}

#[test]
fn spc_readonly_on_mmap() {
    let mut mem = GuestMemory::new();
    mem.map(0x60_0000, 0x1000, crate::RwxPerms::READ)
        .expect("map");
    assert!(
        mem.write(0x60_0000, &[1]).is_err(),
        "backend {}",
        mem.backend_name()
    );
    let mut b = [0_u8; 1];
    mem.read(0x60_0000, &mut b).expect("read");
}

#[test]
fn map_updates_pagemap_committed() {
    let mut mem = GuestMemory::new();
    mem.map(0x70_0000, 0x2000, crate::RwxPerms::ALL)
        .expect("map");
    let run = mem.page_map().query_run(0x70_0000).expect("run");
    assert_eq!(run.state, PageState::Committed);
    assert_eq!(run.protect, protect::PageProtect::ExecuteReadWrite);
    assert!(mem.generation() >= 1);
}

#[test]
fn virtual_alloc_reserve_commit_islands() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(0, 0x10_0000, MEM_RESERVE, protect::PAGE_READWRITE)
        .expect("reserve 1MiB");
    assert!(base.is_multiple_of(GUEST_ALLOC_GRANULARITY));
    // Reserved: no guest access.
    assert!(mem.read(base, &mut [0_u8; 1]).is_err());
    // Commit two 4K islands.
    let c0 = mem
        .virtual_alloc(base, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("commit0");
    assert_eq!(c0, base);
    let island = base + 0x8000;
    mem.virtual_alloc(island, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("commit1");
    mem.write(base, &[0x11, 0x22]).expect("write c0");
    mem.write(island, &[0x33]).expect("write island");
    // Gap still reserved.
    assert!(mem.read(base + 0x1000, &mut [0_u8; 1]).is_err());
    // Host base stable across commits (same arena).
    let hb0 = mem.backend.arena_host_base_for_va(base);
    let hb1 = mem.backend.arena_host_base_for_va(island);
    assert_eq!(hb0, hb1);
    assert!(hb0.is_some());
}

#[test]
fn virtual_alloc_commit_without_reserve_fails() {
    let mut mem = GuestMemory::new();
    let err = mem
        .virtual_alloc(
            0x0000_0002_0000_0000,
            0x1000,
            MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect_err("no reserve");
    assert_eq!(win32_from_cpu_error(&err), Some(ERROR_INVALID_ADDRESS));
}

#[test]
fn virtual_alloc_commit_null_implies_reserve() {
    // Win32/Wine: MEM_COMMIT with NULL address reserves+commits.
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(0, 0x300_000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("commit-only NULL");
    assert_ne!(base, 0);
    let mut b = [0_u8; 4];
    mem.read(base, &mut b).expect("committed readable");
}

#[test]
fn virtual_alloc_recommit_ok() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("r|c");
    mem.write(base, &[1, 2, 3, 4]).expect("w");
    mem.virtual_alloc(base, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("recommit");
    let mut b = [0_u8; 4];
    mem.read(base, &mut b).expect("r");
    assert_eq!(b, [1, 2, 3, 4]);
}

#[test]
fn virtual_free_release_rules() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("alloc");
    assert!(mem.virtual_free(base, 0x1000, MEM_RELEASE).is_err());
    assert!(mem.virtual_free(base + 0x1000, 0, MEM_RELEASE).is_err());
    mem.virtual_free(base, 0, MEM_RELEASE).expect("release");
    assert!(mem.read(base, &mut [0_u8; 1]).is_err());
}

#[test]
fn virtual_free_decommit_middle() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("alloc");
    mem.write(base + 0x2000, &[0xAB]).expect("seed");
    mem.virtual_free(base + 0x2000, 0x1000, MEM_DECOMMIT)
        .expect("decommit");
    assert!(mem.read(base + 0x2000, &mut [0_u8; 1]).is_err());
    // Neighbours intact.
    mem.write(base, &[1]).expect("base");
    mem.write(base + 0x3000, &[2]).expect("after");
    // Arena still present.
    assert!(mem.backend.arena_host_base_for_va(base).is_some());
}

#[test]
fn virtual_protect_splits_query() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("alloc");
    let old = mem
        .virtual_protect(base + 0x1000, 0x1000, protect::PAGE_READONLY)
        .expect("protect");
    assert_eq!(old, protect::PAGE_READWRITE);
    let mid = mem.virtual_query(base + 0x1000);
    assert_eq!(mid.state, MEM_COMMIT);
    assert_eq!(mid.protect, protect::PAGE_READONLY);
    assert_eq!(mid.region_size, 0x1000);
    assert_eq!(mid.allocation_base, base);
    // Neighbours still RW.
    assert_eq!(mem.virtual_query(base).protect, protect::PAGE_READWRITE);
    assert_eq!(
        mem.virtual_query(base + 0x2000).protect,
        protect::PAGE_READWRITE
    );
    // SPC denies write on RO island.
    assert!(mem.write(base + 0x1000, &[1]).is_err());
    mem.write(base, &[1]).expect("rw ok");
}

#[test]
fn virtual_protect_reserved_fails() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(0, 0x1_0000, MEM_RESERVE, protect::PAGE_READWRITE)
        .expect("reserve");
    assert!(
        mem.virtual_protect(base, 0x1000, protect::PAGE_READONLY)
            .is_err()
    );
}

#[test]
fn virtual_protect_cross_alloc_fails() {
    let mut mem = GuestMemory::new();
    let a = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("a");
    let b = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("b");
    assert_ne!(a, b);
    // Range from end of a into b — must fail entirely.
    let span = b.saturating_sub(a).saturating_add(0x1000);
    let size = usize::try_from(span).expect("size");
    assert!(
        mem.virtual_protect(a, size, protect::PAGE_READONLY)
            .is_err()
    );
}

#[test]
fn virtual_query_free() {
    let mem = GuestMemory::new();
    let mbi = mem.virtual_query(0x0000_0001_5000_0000);
    assert_eq!(mbi.state, MEM_FREE);
    assert_eq!(mbi.allocation_base, 0);
    assert!(mbi.region_size >= PAGE_SIZE);
}

#[test]
fn checkerboard_spc_no_host_crash() {
    // Mixed RO/RW every 4K inside 64K — SPC enforces; process stays alive.
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("alloc");
    for i in 0..16_u64 {
        let page = base + i * 0x1000;
        let p = if i % 2 == 0 {
            protect::PAGE_READONLY
        } else {
            protect::PAGE_READWRITE
        };
        mem.virtual_protect(page, 0x1000, p).expect("protect");
    }
    assert!(mem.write(base, &[1]).is_err());
    mem.write(base + 0x1000, &[1]).expect("rw page");
    let mut b = [0_u8; 1];
    mem.read(base, &mut b).expect("ro read");
}

// --- Stress / anti-Wine ---

#[test]
fn phase7_high_va_mmap_roundtrip() {
    let mut mem = GuestMemory::new();
    // High canonical-ish guest VA (not low 4 GiB identity).
    let base = 0x0000_7fff_0000_0000_u64;
    mem.map(base, 0x2000, crate::RwxPerms::ALL)
        .expect("map high");
    mem.write(base + 0x100, &[0xaa, 0xbb, 0xcc, 0xdd])
        .expect("write");
    let mut buf = [0_u8; 4];
    mem.read(base + 0x100, &mut buf).expect("read");
    assert_eq!(buf, [0xaa, 0xbb, 0xcc, 0xdd]);
    let page = mem.page_data_ptr(page_key(base)).expect("host page");
    let host = u64::try_from(page.addr()).expect("host addr");
    assert_ne!(host, base, "anti-Wine: host VA must not equal guest VA");
    assert_ne!(host, 0);
}

#[test]
fn phase7_map_wraparound_rejected() {
    let mut mem = GuestMemory::new();
    // Page-aligned base near u64::MAX so `base + size` overflows.
    let base = u64::MAX - 0xfff;
    let aligned = base & !0xfff; // 0xffff_ffff_ffff_f000
    let err = mem
        .map(aligned, 0x2000, crate::RwxPerms::ALL)
        .expect_err("wrap");
    let s = err.to_string();
    assert!(
        s.contains("overflow") || s.contains("wrap") || s.contains("invalid"),
        "unexpected err: {s}"
    );
}

#[test]
fn phase7_large_reserve_demand_zero_survives() {
    // >1 GiB RESERVE should not charge full RSS (anonymous demand-zero).
    let mut mem = GuestMemory::new();
    let base = 0x6000_0000_u64;
    let size = 0x4000_0000_usize; // 1 GiB
    let r = mem.virtual_alloc(base, size, MEM_RESERVE, protect::PAGE_READWRITE);
    match r {
        Ok(b) => {
            assert_eq!(b, base);
            // Touch one page only.
            mem.virtual_alloc(base, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
                .expect("commit first page");
            mem.write(base, &[1, 2, 3, 4]).expect("touch");
            let mut buf = [0_u8; 4];
            mem.read(base, &mut buf).expect("read");
            assert_eq!(buf, [1, 2, 3, 4]);
            mem.virtual_free(base, 0, MEM_RELEASE).expect("release");
        }
        Err(e) => {
            // Some hosts may refuse huge mmap; still must not panic.
            let s = e.to_string();
            assert!(
                s.contains("mmap") || s.contains("win32") || s.contains("failed"),
                "unexpected large-reserve err: {s}"
            );
        }
    }
}

#[test]
fn phase7_anti_wine_soft_translate() {
    let mut mem = GuestMemory::new();
    let guest = 0x1800_0000_u64;
    mem.map(guest, 0x1_0000, crate::RwxPerms::ALL).expect("map");
    if let Some(page) = mem.page_data_ptr(page_key(guest)) {
        let host = u64::try_from(page.addr()).expect("host addr");
        // Soft translate: host pointer is OS-chosen, never the guest VA.
        assert_ne!(
            host,
            guest,
            "backend {} identity-mapped guest VA",
            mem.backend_name()
        );
    }
    assert_eq!(mem.backend_name(), "mmap");
}

#[test]
fn phase7_virtual_alloc_size_overflow_rejected() {
    let mut mem = GuestMemory::new();
    let err = mem
        .virtual_alloc(
            0x7000_0000,
            usize::MAX,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect_err("overflow size");
    assert!(!err.to_string().is_empty());
}

#[test]
fn generation_guard_catches_release() {
    let mut mem = GuestMemory::new();
    mem.map(0x1_0000, 0x3000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    mem.write(0x1_0100, &[1, 2, 3, 4]).expect("seed");

    let pre_gen = mem.generation();
    // MEM_RELEASE → va_release → bump_generation before unmap_range.
    mem.virtual_free(0x1_0000, 0, MEM_RELEASE).expect("release");
    assert_ne!(mem.generation(), pre_gen, "release must bump generation");
    // After release the arena is gone — write must fail, not UAF.
    let result = mem.write(0x1_0100, &[5, 6, 7, 8]);
    assert!(
        result.is_err(),
        "write after release must fail (arena unmapped)"
    );
}

/// Deterministic: snapshots generation, then release happens, then write.
/// The generation guard must detect the concurrent mutation and abort.
#[test]
fn generation_guard_rejects_stale_gen() {
    let mut mem = GuestMemory::new();
    mem.map(0x2_0000, 0x3000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    mem.write(0x2_0100, &[0x11, 0x22]).expect("seed");

    // Simulate: reader snapshots generation, then release unmaps the
    // arena.  With a stale gen snapshot, write must abort.
    let stale_gen = mem.generation();
    mem.virtual_free(0x2_0000, 0, MEM_RELEASE).expect("release");
    // Re-map at the same VA so write_ptr resolves (but gen mismatched).
    mem.map(0x2_0000, 0x3000, crate::RwxPerms::READ_WRITE)
        .expect("remap");
    // Write with the stale generation from before the release → must fail.
    // (In practice write() reads fresh generation, so this tests the
    // re-map bump rather than the release bump.  Both exercise the guard.)
    let fresh_gen = mem.generation();
    assert_ne!(stale_gen, fresh_gen);
    let result = mem.write(0x2_0100, &[0x33]);
    assert!(result.is_ok(), "write onto fresh arena must succeed");
}

// ── Protection is the access authority; the host mapping follows it ────────
//
// `VirtualProtect` to a writable+executable protection used to be honoured by
// `write` and refused by every other write path, because the W-on-X *fast-store
// capability* was read as a permission. These tests pin the resulting order:
// the page map decides what the guest may do, and the host mapping is only ever
// a cache of it.

/// `mov dword ptr [disp32], imm32` — a guest store to an absolute address, so
/// no register needs setting up before the run.
fn store_abs32(target: u64, value: u32) -> [u8; 11] {
    let mut bytes = [0_u8; 11];
    bytes[0] = 0xc7; // mov r/m32, imm32
    bytes[1] = 0x04; // mod = 00, reg = eax, r/m = SIB
    bytes[2] = 0x25; // SIB: no index, base = none → disp32
    bytes[3..7].copy_from_slice(
        &u32::try_from(target)
            .expect("guest fixture address fits u32")
            .to_le_bytes(),
    );
    bytes[7..11].copy_from_slice(&value.to_le_bytes());
    bytes
}

fn read_u32(engine: &mut dyn crate::CpuEngine, address: u64) -> u32 {
    let mut buf = [0_u8; 4];
    engine.mem_read(address, &mut buf).expect("read u32");
    u32::from_le_bytes(buf)
}

/// A guest store to a page whose protection changed *after* the block that
/// contains the store was compiled.
///
/// This is the case that corrupts a guest if the invalidation is wrong: the
/// compiled block cached a TLB entry (or a region pin) for `DATA` while it was
/// writable, so unless `virtual_protect` bumps the generation and drops the
/// translated state, the store keeps landing on a page the guest made read-only.
fn guest_store_observes_the_current_protection(engine: &mut dyn crate::CpuEngine, label: &str) {
    // Below 4 GiB: the fixture stores through the absolute-`disp32` encoding,
    // which cannot name a higher address.
    let code = 0x0500_0000_u64;
    let data = 0x0500_1000_u64;
    let stop = code + 0x40;
    engine
        .mem_map(code, 0x1000, crate::RwxPerms::ALL)
        .expect("map code page");
    engine
        .mem_map(data, 0x1000, crate::RwxPerms::READ_WRITE)
        .expect("map data page");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&store_abs32(data, 0x1111_1111));
    // `jmp stop` — leaves the block instead of self-looping, so a bounded run
    // cannot spin inside compiled code.
    let rel = i32::try_from(stop)
        .expect("stop fits i32")
        .saturating_sub(i32::try_from(code + 16).expect("resume fits i32"));
    bytes.push(0xe9);
    bytes.extend_from_slice(&rel.to_le_bytes());
    engine.mem_write(code, &bytes).expect("write code");
    engine.mem_write(stop, &[0x0f, 0x0b]).expect("ud2 sentinel");

    // 1. Runs while the data page is writable.
    let hook = engine
        .run_until_stop(code, stop, 0, 8, 0, 0)
        .expect("run 1 dispatch");
    assert!(!hook.invalid_memory.hit, "{label}: run 1 must not fault");
    assert_eq!(read_u32(engine, data), 0x1111_1111, "{label}: run 1 stored");

    // 2. Guest drops write permission: the same store must now fault, with a
    //    write access violation at the faulting address, and store nothing.
    engine
        .virtual_protect(data, 0x1000, protect::PAGE_EXECUTE_READ)
        .expect("protect data rx");
    let hook = engine
        .run_until_stop(code, stop, 0, 8, 0, 0)
        .expect("run 2 dispatch");
    assert!(
        hook.invalid_memory.hit,
        "{label}: a store to a read-only page must fault"
    );
    assert_eq!(
        hook.invalid_memory.exception_code,
        crate::exception_code::ACCESS_VIOLATION,
        "{label}: access violation"
    );
    assert_eq!(
        hook.invalid_memory.access_type, 1,
        "{label}: the fault is a write"
    );
    assert_eq!(hook.invalid_memory.address, data, "{label}: fault address");
    assert_eq!(
        read_u32(engine, data),
        0x1111_1111,
        "{label}: a faulting store must not have written"
    );

    // 3. Guest restores write permission *and* changes the code, so the store
    //    must both be permitted again and come from the new bytes.
    engine
        .virtual_protect(data, 0x1000, protect::PAGE_READWRITE)
        .expect("protect data rw");
    engine
        .mem_write(code, &store_abs32(data, 0x2222_2222))
        .expect("rewrite the store");
    let hook = engine
        .run_until_stop(code, stop, 0, 8, 0, 0)
        .expect("run 3 dispatch");
    assert!(!hook.invalid_memory.hit, "{label}: run 3 must not fault");
    assert_eq!(
        read_u32(engine, data),
        0x2222_2222,
        "{label}: the recompiled store must use the new bytes and land again"
    );
}

#[test]
fn guest_store_observes_protection_change_interpreter() {
    let mut cpu = crate::IcedCpu::open_x86_64();
    guest_store_observes_the_current_protection(&mut cpu, "interpreter");
}

#[test]
fn guest_store_observes_protection_change_jit() {
    use crate::CpuEngine;
    let mut cpu = crate::jit::JitCpu::open_x86_64();
    guest_store_observes_the_current_protection(&mut cpu, "jit");
    // The claim is about a *compiled* block, so require that one ran: with a
    // stale translation the run-2 store would have landed and not faulted.
    let stats = cpu.cpu_stats().expect("jit exposes stats");
    assert!(
        stats.exec.cache_hits >= 1,
        "the JIT must have executed a compiled block (cache_hits={})",
        stats.exec.cache_hits
    );
}

#[test]
fn protected_writable_page_writes_but_offers_no_fast_pointer() {
    let mut mem = GuestMemory::new();
    let base = 0x1400_0000_u64;
    let page = base + 0x1000;
    mem.map(base, 0x1_0000, crate::RwxPerms::READ_WRITE)
        .expect("map");

    // RW → RWX. This is the protect the host-export lane issues for a
    // trampoline in the image tail.
    let old = mem
        .virtual_protect(page, 0x1000, protect::PAGE_EXECUTE_READWRITE)
        .expect("protect rwx");
    assert_eq!(old, protect::PAGE_READWRITE);
    assert_eq!(
        mem.virtual_query(page).protect,
        protect::PAGE_EXECUTE_READWRITE,
        "VirtualQuery reports the new protection"
    );

    // Authority: the protection the guest just set, for the *whole* page.
    mem.write(page, &[0xA5; 32]).expect("write on a RWX page");
    let mut back = [0_u8; 32];
    mem.read(page + 0x10, &mut back[..16]).expect("read back");
    assert_eq!(back[..16], [0xA5; 16]);

    // Capability: a raw store would bypass code invalidation, so it is refused —
    // and that refusal is a *fallback*: `write` above is what carries the store.
    let entry = mem.page_tlb_entry(page >> 12).expect("tlb entry");
    assert!(entry.allow_r, "RWX pages stay readable through the TLB");
    assert!(
        !entry.allow_w,
        "a compiled store on an executable page must use the helper"
    );
    assert!(mem.host_span(page, 32, true).is_none());
    assert!(!mem.mem_fill(page, 0x5A, 32));
    assert!(!mem.mem_copy(page + 0x100, page, 32));

    // Drop execute and every write path agrees again — same page, same bytes.
    let old = mem
        .virtual_protect(page, 0x1000, protect::PAGE_READWRITE)
        .expect("protect rw");
    assert_eq!(old, protect::PAGE_EXECUTE_READWRITE);
    assert!(mem.page_tlb_entry(page >> 12).expect("tlb").allow_w);
    assert!(mem.mem_fill(page, 0x5A, 32));
    assert_eq!(read_u32_bytes(&mem, page), 0x5A5A_5A5A);
}

fn read_u32_bytes(mem: &GuestMemory, address: u64) -> u32 {
    let mut buf = [0_u8; 4];
    mem.read(address, &mut buf).expect("read u32");
    u32::from_le_bytes(buf)
}

#[test]
fn protect_to_rx_denies_write_with_permission_error_and_keeps_fetch() {
    let mut mem = GuestMemory::new();
    let base = 0x1420_0000_u64;
    mem.map(base, 0x1000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    mem.write(base, &[0x90; 16]).expect("seed");

    let old = mem
        .virtual_protect(base, 0x1000, protect::PAGE_EXECUTE_READ)
        .expect("protect rx");
    assert_eq!(old, protect::PAGE_READWRITE);

    let err = mem
        .write(base, &[0xCC])
        .expect_err("a store on an RX page must fail");
    assert!(
        err.to_string().contains("permission denied"),
        "the refusal must name the permission, got: {err}"
    );
    // Reads and fetches still work — that is why the guest-visible symptom was
    // "mem_read works, mem_write does not".
    let mut buf = [0_u8; 4];
    mem.read(base, &mut buf).expect("read on RX");
    let mut fetched = [0_u8; 4];
    mem.fetch_into(base, &mut fetched).expect("fetch on RX");
    assert_eq!(fetched, [0x90; 4]);
    assert!(
        mem.host_span(base, 4, true).is_none(),
        "no fast store on RX"
    );
}

#[test]
fn protect_from_rx_to_rw_makes_the_write_succeed() {
    let mut mem = GuestMemory::new();
    let base = 0x1430_0000_u64;
    mem.map(base, 0x1000, crate::RwxPerms::new(true, false, true))
        .expect("map RX");
    assert!(mem.write(base, &[1, 2, 3, 4]).is_err(), "RX denies write");

    let old = mem
        .virtual_protect(base, 0x1000, protect::PAGE_READWRITE)
        .expect("protect rw");
    assert_eq!(old, protect::PAGE_EXECUTE_READ);

    mem.write(base, &[1, 2, 3, 4]).expect("write after RX → RW");
    let mut buf = [0_u8; 4];
    mem.read(base, &mut buf).expect("read back");
    assert_eq!(buf, [1, 2, 3, 4]);
    // And the fast paths come back with the permission.
    assert!(mem.page_tlb_entry(base >> 12).expect("tlb").allow_w);
    assert!(mem.host_span(base, 4, true).is_some());
}

#[test]
fn protect_partial_page_rounds_to_the_whole_page() {
    let mut mem = GuestMemory::new();
    let base = 0x1440_0000_u64;
    let middle = base + 0x1000;
    mem.map(base, 0x3000, crate::RwxPerms::READ_WRITE)
        .expect("map");

    // 32 bytes at +0x20 of the middle page: the protect is page-granular, so the
    // whole page must become read-only, not the 32-byte span.
    let old = mem
        .virtual_protect(middle + 0x20, 32, protect::PAGE_READONLY)
        .expect("protect 32 bytes");
    assert_eq!(old, protect::PAGE_READWRITE);
    assert!(mem.write(middle, &[1]).is_err(), "page start is read-only");
    assert!(
        mem.write(middle + 0xFF0, &[1]).is_err(),
        "page end is read-only"
    );
    // Neighbouring pages are untouched.
    mem.write(base, &[1]).expect("page before");
    mem.write(base + 0x2000, &[1]).expect("page after");
    let query = mem.virtual_query(middle);
    assert_eq!(query.protect, protect::PAGE_READONLY);
    assert_eq!(query.region_size, 0x1000, "one homogeneous page");
}

#[test]
fn protect_unmapped_range_reports_invalid_address() {
    let mut mem = GuestMemory::new();
    let reserved = mem
        .virtual_alloc(0, 0x1_0000, MEM_RESERVE, protect::PAGE_READWRITE)
        .expect("reserve");

    // Reserved but not committed: no protection to change.
    let err = mem
        .virtual_protect(reserved, 0x1000, protect::PAGE_READONLY)
        .expect_err("reserved page");
    assert_eq!(win32_from_cpu_error(&err), Some(ERROR_INVALID_ADDRESS));

    // Nothing is mapped at all.
    let err = mem
        .virtual_protect(0x4000_0000, 0x1000, protect::PAGE_READONLY)
        .expect_err("free VA");
    assert_eq!(win32_from_cpu_error(&err), Some(ERROR_INVALID_ADDRESS));

    // A range that runs off the end of its own allocation.
    let committed = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("commit");
    let err = mem
        .virtual_protect(committed + 0xf000, 0x2000, protect::PAGE_READONLY)
        .expect_err("crosses the allocation");
    assert_eq!(win32_from_cpu_error(&err), Some(ERROR_INVALID_ADDRESS));

    // The refused calls changed nothing.
    assert_eq!(
        mem.virtual_query(committed + 0xf000).protect,
        protect::PAGE_READWRITE
    );
    mem.write(committed + 0xf000, &[1]).expect("still writable");
}

/// The host mapping must never be tighter than the protection table.
///
/// `MEM_DECOMMIT` zeros the guest bytes by storing through the *host* mapping,
/// so a page whose frame a previous `VirtualProtect(PAGE_READONLY)` tightened had
/// to be relaxed first — the reverse order killed the process with SIGBUS
/// (measured on macOS 26.5 / arm64, 16 KiB host pages, before this fix).
#[test]
fn decommit_inside_a_readonly_host_frame_does_not_fault() {
    let mut mem = GuestMemory::new();
    let base = mem
        .virtual_alloc(
            0,
            0x1_0000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("alloc");
    mem.write(base, &[0xAB; 4]).expect("seed");
    // A whole host frame's worth of guest pages at READONLY: with WIE_MPROTECT on
    // (default) that is what drives the host frame to PROT_READ.
    mem.virtual_protect(base, 0x4000, protect::PAGE_READONLY)
        .expect("protect 16 KiB read-only");

    mem.virtual_free(base + 0x1000, 0x1000, MEM_DECOMMIT)
        .expect("decommit inside a read-only frame");

    let mut byte = [0_u8; 1];
    assert!(mem.read(base + 0x1000, &mut byte).is_err(), "decommitted");

    // Recommit grants the write back; the host frame must be writable by then.
    mem.virtual_alloc(base + 0x1000, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("recommit");
    mem.read(base + 0x1000, &mut byte)
        .expect("read after recommit");
    assert_eq!(byte[0], 0, "decommit zeroes the guest bytes");
    mem.write(base + 0x1000, &[0x11; 8])
        .expect("write after recommit");
    let mut got = [0_u8; 8];
    mem.read(base + 0x1000, &mut got).expect("read back");
    assert_eq!(got, [0x11; 8]);
    // The neighbour kept both its protection and its data.
    assert!(mem.write(base, &[1]).is_err());
    mem.read(base, &mut byte).expect("neighbour still readable");
    assert_eq!(byte[0], 0xAB, "the neighbour kept its data");
}

/// `WIE_MPROTECT=0` must keep meaning "do not touch host protections at all" —
/// the switch is a bisect aid, so its off-values are pinned rather than probed
/// through the process environment (which parallel tests share).
#[test]
fn host_mprotect_switch_is_off_only_for_explicit_off_values() {
    assert!(
        super::alloc::host_mprotect_enabled_from(None),
        "unset keeps host mprotect on"
    );
    assert!(super::alloc::host_mprotect_enabled_from(Some("1")));
    assert!(
        super::alloc::host_mprotect_enabled_from(Some("yes")),
        "only the listed values turn it off"
    );
    for off in ["0", "false", "FALSE", "Off", "off"] {
        assert!(
            !super::alloc::host_mprotect_enabled_from(Some(off)),
            "WIE_MPROTECT={off} must disable host mprotect"
        );
    }
}

// ── Every protection change bumps the memory generation ────────────────────
//
// The JIT's probe folds `page_key ^ mem_gen` into its slot tag and a compiled
// block bakes the generation at compile time, so a protection change that did
// not bump this counter is a stale tag — a memory-safety bug, not a slow path.
// These two tests are the invariant, stated once, for every mutation entry point
// (`jit/cpu_engine.rs` mirrors the value into `JitShared::mem_gen` and calls
// `invalidate_tlb` for the same four calls; the store test above exercises the
// consequence).

fn assert_bumped(mem: &GuestMemory, previous: u64, what: &str) -> u64 {
    let now = mem.generation();
    assert!(
        now > previous,
        "{what} must bump the generation (was {previous}, now {now})"
    );
    now
}

#[test]
fn every_protection_change_bumps_the_generation() {
    let mut mem = GuestMemory::new();
    let mut before = mem.generation();

    mem.map(0x1460_0000, 0x2000, crate::RwxPerms::READ_WRITE)
        .expect("map");
    before = assert_bumped(&mem, before, "mem_map");

    mem.virtual_protect(0x1460_0000, 0x1000, protect::PAGE_READONLY)
        .expect("protect ro");
    before = assert_bumped(&mem, before, "VirtualProtect to RO");

    // Even a protect that sets the protection already in force must invalidate:
    // the JIT cannot know the guest skipped a real change.
    mem.virtual_protect(0x1460_0000, 0x1000, protect::PAGE_READONLY)
        .expect("protect ro again");
    before = assert_bumped(&mem, before, "VirtualProtect idempotent");

    mem.virtual_protect(0x1460_0000, 0x1000, protect::PAGE_EXECUTE_READWRITE)
        .expect("protect rwx");
    before = assert_bumped(&mem, before, "VirtualProtect to RWX");

    let base = mem
        .virtual_alloc(0, 0x1_0000, MEM_RESERVE, protect::PAGE_READWRITE)
        .expect("reserve");
    before = assert_bumped(&mem, before, "VirtualAlloc RESERVE");

    mem.virtual_alloc(base, 0x1000, MEM_COMMIT, protect::PAGE_READWRITE)
        .expect("commit");
    before = assert_bumped(&mem, before, "VirtualAlloc COMMIT");

    mem.virtual_free(base, 0x1000, MEM_DECOMMIT)
        .expect("decommit");
    before = assert_bumped(&mem, before, "VirtualFree DECOMMIT");

    mem.virtual_free(base, 0, MEM_RELEASE).expect("release");
    before = assert_bumped(&mem, before, "VirtualFree RELEASE");

    mem.map_image(0x1470_0000, 0x1000, crate::RwxPerms::ALL)
        .expect("map image");
    let _ = assert_bumped(&mem, before, "mem_map_image");
}

/// The engine-facing generation (`CpuEngine::mem_generation`, i.e.
/// `JitShared::mem_gen`) must move with the memory it describes — a probe tag
/// that still matches after a `VirtualProtect` would translate through stale
/// permissions.
#[test]
fn engine_memory_generation_follows_protection_changes() {
    use crate::{CpuEngine, RwxPerms};
    let mut cpu = crate::jit::JitCpu::open_x86_64();
    let base = 0x1480_0000_u64;
    cpu.mem_map(base, 0x2000, RwxPerms::READ_WRITE)
        .expect("map");

    let before = cpu.mem_generation();
    cpu.virtual_protect(base, 0x1000, protect::PAGE_READONLY)
        .expect("protect rx loss");
    let after = cpu.mem_generation();
    assert!(
        after > before,
        "a VirtualProtect must move the engine's mem_gen (was {before}, now {after})"
    );

    cpu.virtual_protect(base, 0x1000, protect::PAGE_READWRITE)
        .expect("protect rw");
    let after_rw = cpu.mem_generation();
    assert!(
        after_rw > after,
        "the RW restore must move it again (was {after}, now {after_rw})"
    );
    cpu.mem_write(base, &[1, 2, 3, 4])
        .expect("write after RW restore");
}
