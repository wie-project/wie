use super::*;

#[test]
fn code_inv_x_loss_drops_ready() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base, base + 16);
    assert!(cpu.has_ready_at(base));
    assert!(cpu.code_pages_overlap(base, 16));

    cpu.virtual_protect(base, 0x1000, protect::PAGE_READONLY)
        .expect("x-loss");
    assert!(!cpu.has_ready_at(base));
    assert!(!cpu.code_pages_overlap(base, 16));
    assert_eq!(cpu.thread.ctx.edge_ic_va[0], 0);
    assert!(cpu.stats().exec.code_invs >= 1);
}

#[test]
fn code_inv_smc_write_drops_ready() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base + 0x10, base + 0x20);
    assert!(cpu.has_ready_at(base + 0x10));

    // Guest/host store into compiled range.
    cpu.mem_write(base + 0x12, &[0x90, 0x90]).expect("smc");
    assert!(!cpu.has_ready_at(base + 0x10));
    assert_eq!(cpu.thread.ctx.edge_ic_fn[0], 0);
}

#[test]
fn code_inv_data_write_leaves_code() {
    let mut cpu = JitCpu::open_x86_64();
    let code = alloc_code_page(&mut cpu);
    let data = 0x1003_0000_u64;
    cpu.virtual_alloc(
        data,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("data");
    cpu.test_plant_ready(code, code + 8);
    let invs = cpu.stats().exec.code_invs;
    cpu.mem_write(data, &[1, 2, 3, 4]).expect("data write");
    assert!(cpu.has_ready_at(code));
    assert_eq!(cpu.stats().exec.code_invs, invs);
    assert_eq!(cpu.thread.ctx.edge_ic_va[0], code);
}

#[test]
fn code_inv_free_drops_ready() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base, base + 4);
    cpu.virtual_free(base, 0, MEM_RELEASE).expect("free");
    assert!(!cpu.has_ready_at(base));
    assert!(cpu.shared.code_pages.lock().unwrap().is_empty());
}

#[test]
fn code_inv_rx_stays_on_x_preserve() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base, base + 8);
    // Keep execute; only drop write — code content unchanged.
    cpu.virtual_protect(base, 0x1000, protect::PAGE_EXECUTE_READ)
        .expect("rx");
    assert!(cpu.has_ready_at(base));
}

#[test]
fn ranges_overlap_half_open() {
    assert!(ranges_overlap(0x10, 0x20, 0x1f, 0x30));
    assert!(!ranges_overlap(0x10, 0x20, 0x20, 0x30));
    assert!(ranges_overlap(0x10, 0x20, 0x00, 0x11));
}

#[test]
fn no_w_tlb_on_executable_page() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    let e = cpu
        .shared
        .mem
        .write()
        .unwrap()
        .page_tlb_entry(base >> 12)
        .expect("tlb");
    assert!(e.allow_r);
    assert!(!e.allow_w);
    let data = 0x1007_0000_u64;
    cpu.virtual_alloc(
        data,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("data");
    let e2 = cpu
        .shared
        .mem
        .write()
        .unwrap()
        .page_tlb_entry(data >> 12)
        .expect("data tlb");
    assert!(e2.allow_r && e2.allow_w);
    let _ = RwxPerms::ALL; // silence if unused in some cfgs
}

// --- Stress residual (invalidation multi-region / FIC) ---

#[test]
fn code_inv_smc_across_page_boundary() {
    let mut cpu = JitCpu::open_x86_64();
    let base = 0x1010_0000_u64;
    map_code_region(&mut cpu, base, 0x2000);
    // Ready block straddles page boundary (last 8 B of page0 + first of page1).
    let entry = base + 0x0ff8;
    cpu.test_plant_ready(entry, entry + 16);
    assert!(cpu.has_ready_at(entry));
    // Store on page1 half of the range.
    cpu.mem_write(base + 0x1000, &[0x90, 0x90]).expect("smc p1");
    assert!(!cpu.has_ready_at(entry));
    assert_eq!(cpu.thread.ctx.edge_ic_va[0], 0);
}

#[test]
fn code_inv_multi_region_protect_and_free() {
    let mut cpu = JitCpu::open_x86_64();
    let a = alloc_code_page(&mut cpu);
    let b = alloc_code_page(&mut cpu);
    for base in [a, b] {
        cpu.test_plant_ready(base, base + 8);
    }
    assert!(cpu.has_ready_at(a) && cpu.has_ready_at(b));
    // X-loss on A only.
    cpu.virtual_protect(a, 0x1000, protect::PAGE_READONLY)
        .expect("protect a");
    assert!(!cpu.has_ready_at(a));
    assert!(cpu.has_ready_at(b));
    assert_eq!(cpu.thread.ctx.edge_ic_va[0], 0); // edge IC cleared on any selective drop
    // Free B.
    cpu.virtual_free(b, 0, MEM_RELEASE).expect("free b");
    assert!(!cpu.has_ready_at(b));
}

#[test]
fn flush_instruction_cache_drops_ready_range() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base, base + 16);
    assert!(cpu.has_ready_at(base));
    cpu.flush_instruction_cache(base, 16).expect("fic");
    assert!(!cpu.has_ready_at(base));
    assert!(cpu.stats().exec.code_invs >= 1);
}

#[test]
fn flush_instruction_cache_size_zero_clears_all() {
    let mut cpu = JitCpu::open_x86_64();
    let a = alloc_code_page(&mut cpu);
    let b = alloc_code_page(&mut cpu);
    for base in [a, b] {
        cpu.test_plant_ready(base, base + 4);
    }
    cpu.flush_instruction_cache(0, 0).expect("fic all");
    assert!(!cpu.has_ready_at(a));
    assert!(!cpu.has_ready_at(b));
    assert!(cpu.shared.code_pages.lock().unwrap().is_empty());
}
