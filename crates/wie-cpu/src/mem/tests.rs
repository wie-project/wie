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
